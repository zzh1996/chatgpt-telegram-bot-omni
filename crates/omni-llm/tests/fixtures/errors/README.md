# Recorded OpenRouter error responses

One real request per case, recorded with `tools/record-openrouter-errors.py`
(2026-11-11; all cases together cost well under $0.01). Each file holds the
HTTP status and the error body, or every SSE `data:` payload of a stream.
Account identifiers are replaced by placeholders (`user_REDACTED`,
`WORKSPACE`, `KEYHASH`).

The tests in `crates/omni-llm/src/openrouter/recorded.rs` pin down how the bot
classifies each case (retried or not, context overflow, normal stop) without
calling any API; a recorded case without a test fails the suite.

Not recorded on purpose: content moderation and refusals are not provoked with
violating prompts — real occurrences are added when they happen (e.g.
`../messages_refusal_fable52.sse`). Volcengine Ark needs its own key.
