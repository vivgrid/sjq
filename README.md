# sjq

**jq, but for AI streaming.**

`jq` is built for whole JSON documents. A streaming LLM response is hundreds of `data: {...}`
lines, each repeating the same id, model and `null`s around a few characters of new text.
sjq reads an OpenAI-compatible `/v1/chat/completions` or `/v1/responses` stream as it
arrives and prints one short line per event with only what that event adds, then how long
everything took.

Before:

```
data: {"id":"021792034294732770d3492782dd10cc0a489fec0a72ead7e3e0d","created":1792034296,"model":"viv-fast-2609","object":"chat.completion.chunk","system_fingerprint":null,"obfuscation":null,"choices":[{"delta":{"reasoning_content":" user","refusal":null},"index":0,"logprobs":null}]}

data: {"id":"021792034294732770d3492782dd10cc0a489fec0a72ead7e3e0d","created":1792034296,"model":"viv-fast-2609","object":"chat.completion.chunk","system_fingerprint":null,"obfuscation":null,"choices":[{"delta":{"content":"Hola","refusal":null},"index":0,"logprobs":null}]}
```

After:

```
meta: id=021792034294732770d3492782dd10cc0a489fec0a72ead7e3e0d created=1792034296 model=viv-fast-2609
data: role=assistant, reasoning_content='The'
data: reasoning_content=' user'
…
data: '¡'
data: 'Hola'
data: '\n'
data: finish_reason=stop, usage: {"prompt_tokens":232,"completion_tokens":44,"total_tokens":276,"completion_tokens_details":{"reasoning_tokens":24}}
data: [DONE]
time: connect=0.3ms ttfb=308.7ms ttft=531.8ms first_content=1.119s total=1.416s speed=49.8tok/s
```

## Install

```sh
cargo install --git https://github.com/fanweixiao/sjq
```

## Usage

```sh
# Let sjq run curl, and you also get connect / tls / ttfb timings
sjq curl -s "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer $API_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"'"$MODEL"'","stream":true,"messages":[{"role":"user","content":"hi"}]}'

# Or pipe into it (-N stops curl from buffering the stream)
curl -sN … | sjq

# Or replay a saved stream
sjq capture.txt
```

Options: `-t` prefixes every line with the seconds elapsed, and `--color auto|always|never`
sets coloring (`NO_COLOR` is honored).

## What you see

| chunk | line |
|---|---|
| content | `data: ' in'` |
| reasoning | `data: reasoning_content=' user'`, with the provider's own field name |
| first delta | `data: role=assistant, '¡'` |
| tool call | `data: tool_calls[0] id=call_1 name=get_weather`, then `data: tool_calls[0] args='{"city":'` |
| last chunk | `data: finish_reason=stop, usage: {...}` |
| error | `data: error: {"message":"…","code":429}` |
| anything else | unrecognized non-null fields show as `key=value`, so nothing unusual is hidden |

- Text is quoted and escaped, so leading spaces, empty deltas (`''`) and newlines are visible
  and every chunk stays on one line.
- `id` and `model` show once per stream on the `meta:` line, and again when they change.
- `usage` drops zero counters inside `*_details`.
- Other lines (`event:`, `: ping` keep-alives, non-SSE error bodies) pass through unchanged,
  except an `event:` line that only repeats the next payload's `type`.

### Responses API

`/v1/responses` streams typed events instead of chunks. sjq prints the event name without
`response.`, and answer text the same way as chat content:

```
meta: created_at=1792038227 id=resp_0217… model=… reasoning={"effort":"medium"} service_tier=auto
data: created status=in_progress
data: output_item.added reasoning id=rs_0217… status=in_progress
data: reasoning_summary_text='Simple'
data: reasoning_summary_text.done
data: output_item.added message role=assistant status=in_progress id=msg_0217…
data: content_part.added output_text
data: 'Here'
data: output_text.done
data: output_item.done message status=completed
data: completed, usage: {"input_tokens":18,"output_tokens":87,"output_tokens_details":{"reasoning_tokens":3},"total_tokens":105}
```

- `item_id`, `output_index`, `content_index` and `summary_index` are only shown when they
  change, for example when two function calls stream at once.
- A `sequence_number` is only shown when it skips or goes backwards.
- The `.done` events repeat the full text at every level (text, part, item, response). sjq
  checks that text against the deltas it saw, hides it when it matches, and prints it in red
  when it doesn't.

## Timing

| field | meaning |
|---|---|
| `connect` `tls` `ttfb` | from curl, only with `sjq curl …` |
| `ttft` | first generated token: reasoning, content or tool call |
| `first_content` | first answer token, when reasoning or a tool call came first |
| `total` | last line received |
| `speed` | `completion_tokens` (or `output_tokens`) `/ (total - ttft)` |

A pipe only carries the response body. With `curl … | sjq`, sjq measures from its own start,
which is when the shell started curl, but it can't see the TCP connect. `sjq curl …` runs
curl with `-N -sS -w …` added (replacing any `-w` of yours) to get connection timings, and
exits with curl's status. Replayed files get no `time:` line.
