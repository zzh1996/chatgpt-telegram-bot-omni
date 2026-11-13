//! Database schema and migrations.
//!
//! Migrations are append-only: never edit a released migration, add a new one.

pub const MIGRATIONS: &[&str] = &[
    // ---- v1
    r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- grammers session storage, one logical session per bot
CREATE TABLE tg_session (
    bot TEXT PRIMARY KEY,
    home_dc INTEGER NOT NULL
);
CREATE TABLE tg_dc (
    bot TEXT NOT NULL,
    dc_id INTEGER NOT NULL,
    option TEXT NOT NULL,              -- JSON DcOption (includes auth key)
    PRIMARY KEY (bot, dc_id)
);
CREATE TABLE tg_peer (
    bot TEXT NOT NULL,
    peer_id INTEGER NOT NULL,          -- grammers PeerId (raw)
    info TEXT NOT NULL,                -- JSON PeerInfo
    PRIMARY KEY (bot, peer_id)
);
CREATE TABLE tg_update_state (
    bot TEXT PRIMARY KEY,
    pts INTEGER NOT NULL,
    qts INTEGER NOT NULL,
    date INTEGER NOT NULL,
    seq INTEGER NOT NULL
);
CREATE TABLE tg_channel_state (
    bot TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    pts INTEGER NOT NULL,
    PRIMARY KEY (bot, channel_id)
);

-- people and places (chat ids use the Bot API "marked" convention)
CREATE TABLE users (
    user_id INTEGER PRIMARY KEY,
    username TEXT,
    first_name TEXT,
    last_name TEXT,
    is_bot INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE TABLE chats (
    chat_id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,                -- private | group | supergroup | channel
    title TEXT,
    username TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE bot_chats (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    status TEXT NOT NULL,              -- member | left
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (bot, chat_id)
);
CREATE TABLE acl (
    scope TEXT NOT NULL CHECK (scope IN ('user', 'chat')),
    id INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('allow', 'deny')),
    note TEXT,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);

-- every Telegram message the bot decided to act on (idempotent intake)
CREATE TABLE inbound (
    id INTEGER PRIMARY KEY,
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    msg_id INTEGER NOT NULL,
    sender_id INTEGER,
    date INTEGER NOT NULL,
    text TEXT NOT NULL DEFAULT '',
    reply_to_msg_id INTEGER,
    grouped_id INTEGER,
    payload TEXT NOT NULL DEFAULT '{}', -- JSON: entities, media descriptors, quote, topic...
    status TEXT NOT NULL DEFAULT 'new', -- new | done | ignored | error
    error TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (bot, chat_id, msg_id)
);
CREATE INDEX inbound_status ON inbound(status);
CREATE INDEX inbound_group ON inbound(bot, chat_id, grouped_id);

-- conversation tree
CREATE TABLE turns (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER REFERENCES turns(id),
    role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
    data TEXT NOT NULL,                -- JSON omni_core::Turn
    spec TEXT,                         -- assistant: JSON resolved request spec
    status TEXT NOT NULL DEFAULT 'done', -- pending | running | done | failed | cancelled
    bot TEXT,
    chat_id INTEGER,
    user_id INTEGER,                   -- requesting user (for user turns: author)
    inbound_id INTEGER REFERENCES inbound(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX turns_parent ON turns(parent_id);
CREATE INDEX turns_status ON turns(status);

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    turn_id INTEGER NOT NULL REFERENCES turns(id),
    n INTEGER NOT NULL,
    status TEXT NOT NULL,              -- running | done | failed | interrupted | cancelled
    request TEXT,                      -- JSON request body with blobs redacted
    output TEXT,                       -- JSON omni_core::Turn (partial or final)
    error TEXT,                        -- JSON error, verbatim upstream details
    upstream_id TEXT,                  -- generation / response id
    usage TEXT,                        -- JSON omni_core::Usage
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    UNIQUE (turn_id, n)
);

-- Telegram messages that belong to turns (any part of any attempt)
CREATE TABLE tg_messages (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    msg_id INTEGER NOT NULL,
    turn_id INTEGER NOT NULL REFERENCES turns(id),
    attempt_id INTEGER REFERENCES attempts(id),
    part_index INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bot, chat_id, msg_id)
);
CREATE INDEX tg_messages_turn ON tg_messages(turn_id);

-- outgoing messages, for idempotent send (random_id) and edit tracking
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY,
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    turn_id INTEGER,
    attempt_id INTEGER,
    part_index INTEGER NOT NULL DEFAULT 0,
    random_id INTEGER NOT NULL,
    reply_to INTEGER,
    msg_id INTEGER,                    -- NULL until the send is confirmed
    state TEXT NOT NULL,               -- pending | sent | deleted
    content_hash TEXT,                 -- hash of the last render applied to Telegram
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (bot, random_id)
);
CREATE INDEX outbox_attempt ON outbox(attempt_id);

-- usage & cost ledger (one row per upstream call)
CREATE TABLE usage_ledger (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    bot TEXT,
    chat_id INTEGER,
    user_id INTEGER,
    turn_id INTEGER,
    attempt_id INTEGER UNIQUE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_tokens INTEGER,
    cached_tokens INTEGER,
    cache_write_tokens INTEGER,
    cost_usd REAL,
    cost_source TEXT,
    raw TEXT
);
CREATE INDEX usage_ts ON usage_ledger(ts);
CREATE INDEX usage_user ON usage_ledger(user_id, ts);
CREATE INDEX usage_chat ON usage_ledger(chat_id, ts);

-- token-bucket limits on spend (USD)
CREATE TABLE quota_rules (
    scope TEXT NOT NULL CHECK (scope IN ('user', 'chat')),
    id INTEGER NOT NULL,               -- 0 = default for the scope
    capacity_usd REAL NOT NULL,
    refill_usd_per_day REAL NOT NULL,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);
CREATE TABLE quota_buckets (
    scope TEXT NOT NULL,
    id INTEGER NOT NULL,
    level_usd REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);

-- content-addressed blobs and Telegram file dedupe
CREATE TABLE blobs (
    sha256 TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mime TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE tg_files (
    kind TEXT NOT NULL,                -- photo | document
    file_id INTEGER NOT NULL,          -- Telegram photo/document id (global)
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    mime TEXT,
    PRIMARY KEY (kind, file_id)
);

-- runtime settings and admin-defined aliases
CREATE TABLE settings (
    scope TEXT NOT NULL,               -- global | chat | user
    id INTEGER NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,               -- JSON
    PRIMARY KEY (scope, id, key)
);
CREATE TABLE aliases (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,                -- e.g. `openrouter:openai/gpt-6+effort=max`
    note TEXT,
    created_by INTEGER,
    created_at INTEGER NOT NULL
);
"#,
    // ---- v2: resumable checkpoints (long decode) per attempt
    r#"
ALTER TABLE attempts ADD COLUMN checkpoint TEXT;   -- JSON: {item, parts, usage, hops, prompt_tokens}
"#,
    // ---- v3: one ledger row per billed upstream call (idempotent by generation id),
    //          and calls whose usage never arrived (looked up later)
    r#"
CREATE TABLE usage_ledger_v3 (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    bot TEXT,
    chat_id INTEGER,
    user_id INTEGER,
    turn_id INTEGER,
    attempt_id INTEGER,
    generation_id TEXT UNIQUE,         -- upstream generation / response id
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_tokens INTEGER,
    cached_tokens INTEGER,
    cache_write_tokens INTEGER,
    cost_usd REAL,
    cost_source TEXT,                  -- reported (in the stream) | fetched (looked up afterwards) | unknown
    raw TEXT
);
INSERT INTO usage_ledger_v3(id, ts, bot, chat_id, user_id, turn_id, attempt_id, provider, model, input_tokens,
    output_tokens, reasoning_tokens, cached_tokens, cache_write_tokens, cost_usd, cost_source, raw)
  SELECT id, ts, bot, chat_id, user_id, turn_id, attempt_id, provider, model, input_tokens,
    output_tokens, reasoning_tokens, cached_tokens, cache_write_tokens, cost_usd, cost_source, raw FROM usage_ledger;
DROP TABLE usage_ledger;
ALTER TABLE usage_ledger_v3 RENAME TO usage_ledger;
CREATE INDEX usage_ts ON usage_ledger(ts);
CREATE INDEX usage_user ON usage_ledger(user_id, ts);
CREATE INDEX usage_chat ON usage_ledger(chat_id, ts);
CREATE INDEX usage_attempt ON usage_ledger(attempt_id);

CREATE TABLE pending_costs (
    generation_id TEXT PRIMARY KEY,
    attempt_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    tries INTEGER NOT NULL DEFAULT 0,
    next_at INTEGER NOT NULL
);
"#,
    // ---- v4: media uploaded for rich messages (photos/documents embedded in replies)
    r#"
CREATE TABLE tg_uploads (
    bot TEXT NOT NULL,
    key TEXT NOT NULL,                 -- media key (blob sha256, + "/filename" for documents)
    kind TEXT NOT NULL,                -- photo | document (what Telegram accepted)
    want TEXT NOT NULL,                -- photo | document (what was asked for)
    media_id INTEGER NOT NULL,
    access_hash INTEGER NOT NULL,
    file_reference BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (bot, key, want)
);
"#,
    // ---- v5: flood accounting: what each message shows, and operations Telegram may still count
    r#"
ALTER TABLE outbox ADD COLUMN shape_chars INTEGER;  -- code points of all text Telegram shows
ALTER TABLE outbox ADD COLUMN shape_top INTEGER;    -- its top-level blocks
CREATE TABLE flood_log (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    at INTEGER NOT NULL,               -- ms: when Telegram's answer arrived
    units INTEGER NOT NULL             -- 1 light, 16 heavy
);
CREATE INDEX flood_log_at ON flood_log(at);
"#,
    // ---- v6: every bot is an independent instance: its own access rules (allowed
    //          places, bans everywhere or in one group), spend limits and runtime aliases.
    //          Existing rows are copied to every bot seen so far.
    r#"
CREATE TABLE access_rules (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,          -- a group, a private chat (= the user's id), or 0 = everywhere
    user_id INTEGER NOT NULL,          -- 0 = everyone there
    status TEXT NOT NULL CHECK (status IN ('allow', 'deny')),
    note TEXT,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (bot, chat_id, user_id)
);
INSERT OR IGNORE INTO access_rules(bot, chat_id, user_id, status, note, set_by, set_at)
  SELECT b.bot,
         CASE WHEN a.scope = 'user' AND a.status = 'deny' THEN 0 ELSE a.id END,
         CASE WHEN a.scope = 'user' AND a.status = 'deny' THEN a.id ELSE 0 END,
         a.status, a.note, a.set_by, a.set_at
  FROM acl a CROSS JOIN (SELECT DISTINCT bot FROM bot_chats) b;
DROP TABLE acl;

CREATE TABLE quota_rules_v6 (
    bot TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('user', 'chat')),
    id INTEGER NOT NULL,               -- 0 = default for the scope
    capacity_usd REAL NOT NULL,
    refill_usd_per_day REAL NOT NULL,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (bot, scope, id)
);
INSERT INTO quota_rules_v6(bot, scope, id, capacity_usd, refill_usd_per_day, set_by, set_at)
  SELECT b.bot, q.scope, q.id, q.capacity_usd, q.refill_usd_per_day, q.set_by, q.set_at
  FROM quota_rules q CROSS JOIN (SELECT DISTINCT bot FROM bot_chats) b;
DROP TABLE quota_rules;
ALTER TABLE quota_rules_v6 RENAME TO quota_rules;

CREATE TABLE quota_buckets_v6 (
    bot TEXT NOT NULL,
    scope TEXT NOT NULL,
    id INTEGER NOT NULL,
    level_usd REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (bot, scope, id)
);
INSERT INTO quota_buckets_v6(bot, scope, id, level_usd, updated_at)
  SELECT b.bot, q.scope, q.id, q.level_usd, q.updated_at
  FROM quota_buckets q CROSS JOIN (SELECT DISTINCT bot FROM bot_chats) b;
DROP TABLE quota_buckets;
ALTER TABLE quota_buckets_v6 RENAME TO quota_buckets;

CREATE TABLE aliases_v6 (
    bot TEXT NOT NULL,
    name TEXT NOT NULL,                -- may be empty: the `$text` prefix
    spec TEXT NOT NULL,
    note TEXT,
    created_by INTEGER,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (bot, name)
);
INSERT INTO aliases_v6(bot, name, spec, note, created_by, created_at)
  SELECT b.bot, a.name, a.spec, a.note, a.created_by, a.created_at
  FROM aliases a CROSS JOIN (SELECT DISTINCT bot FROM bot_chats) b;
DROP TABLE aliases;
ALTER TABLE aliases_v6 RENAME TO aliases;
CREATE INDEX usage_bot_chat ON usage_ledger(bot, chat_id, ts);
"#,
    // ---- v7: access and spend limits form a tree, bot → chat → person: bans and
    //          limits live inside one chat (groups and private chats alike);
    //          bot-wide bans and per-person limits across chats are gone.
    r#"
DELETE FROM access_rules WHERE chat_id = 0;

CREATE TABLE quota_rules_v7 (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,          -- a group, or a private chat (= the user's id)
    target TEXT NOT NULL CHECK (target IN ('chat', 'member')),
    user_id INTEGER NOT NULL,          -- 'chat': 0; 'member': the person, or 0 = each member by default
    capacity_usd REAL NOT NULL,
    refill_usd_per_day REAL NOT NULL,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (bot, chat_id, target, user_id)
);
INSERT INTO quota_rules_v7(bot, chat_id, target, user_id, capacity_usd, refill_usd_per_day, set_by, set_at)
  SELECT bot, id, 'chat', 0, capacity_usd, refill_usd_per_day, set_by, set_at
  FROM quota_rules WHERE scope = 'chat' AND id <> 0;
DROP TABLE quota_rules;
ALTER TABLE quota_rules_v7 RENAME TO quota_rules;

CREATE TABLE quota_buckets_v7 (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    target TEXT NOT NULL,
    user_id INTEGER NOT NULL,          -- 'chat': 0; 'member': the person
    level_usd REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (bot, chat_id, target, user_id)
);
INSERT INTO quota_buckets_v7(bot, chat_id, target, user_id, level_usd, updated_at)
  SELECT bot, id, 'chat', 0, level_usd, updated_at FROM quota_buckets WHERE scope = 'chat' AND id <> 0;
DROP TABLE quota_buckets;
ALTER TABLE quota_buckets_v7 RENAME TO quota_buckets;
"#,
    // ---- v8: chat prefixes, custom chatbots members define for one chat
    r#"
CREATE TABLE chat_prefixes (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    name TEXT NOT NULL,                -- lowercase letters and digits
    spec TEXT NOT NULL,                -- the model with all its options, resolved at creation
    turns TEXT NOT NULL,               -- JSON [Turn]: system prompt and history before each new message
    created_by INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (bot, chat_id, name)
);
"#,
    // ---- v9: what each upstream call of an attempt reported about itself
    r#"
ALTER TABLE attempts ADD COLUMN meta TEXT;  -- JSON {calls: [omni_llm::meta::CallMeta], first_output_ms}
"#,
];
