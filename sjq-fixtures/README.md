# sjq 测试数据 — chat completions 流式响应全覆盖

15 个 fixture 文件，覆盖 OpenAI-compatible `/v1/chat/completions`（`stream: true`）响应的各种可能性。每个文件都是一段完整、可 replay 的 SSE 流，直接用 sjq 跑：

```sh
sjq 01-basic-content.txt
# 或
cat 01-basic-content.txt | sjq
```

## 文件一览

| 文件 | 覆盖的场景 | 期望的 sjq 渲染要点 |
|---|---|---|
| `01-basic-content.txt` | 最基础：role → content deltas → `finish_reason=stop` → usage chunk（`choices: []`）→ `[DONE]` | `meta:` 只出现一次；`data: role=assistant, 'Hello'`；`data: ' there'`；`finish_reason=stop, usage: {...}` |
| `02-reasoning.txt` | DeepSeek 式 `reasoning_content` 先行，再出 content（测 `first_content` timing） | `reasoning_content='…'` 用 provider 自己的字段名；usage 里 `reasoning_tokens` 保留 |
| `03-tool-call.txt` | 单个 tool call：首 chunk 带 `id`/`name`，后续 chunk 只有 `arguments` 碎片 | `tool_calls[0] id=call_… name=get_weather`，然后 `args='…'` 逐段 |
| `04-parallel-tool-calls.txt` | 两个 tool call 交错（index 0/1） | 两组 id/name/args 各自归位，不串 |
| `05-refusal.txt` | `refusal` 非空（模型拒绝） | refusal 文本正常显示；`finish_reason=stop` |
| `06-logprobs.txt` | 非空 `logprobs`（`token`/`logprob`/`bytes`/`top_logprobs`） | 内容照常渲染；logprobs 不炸（sjq 可自行决定展示粒度） |
| `07-finish-length.txt` | `finish_reason=length`（max_tokens 截断） | `finish_reason=length` 原样显示 |
| `08-finish-content-filter.txt` | `finish_reason=content_filter` | `finish_reason=content_filter` 原样显示 |
| `09-usage-details.txt` | usage 的 `*_details`：`cached_tokens:100`、`reasoning_tokens:30` 与一堆 0 | 0 值计数器被丢掉，只剩 `cached_tokens` 和 `reasoning_tokens` |
| `10-midstream-error.txt` | 流中间出现 `{"error": {...}}`，然后 `[DONE]` | `data: error: {"message":"…",…}` |
| `11-sse-framing.txt` | SSE framing 边界：`: ping` 注释、`event: message` 行、多行 `data:` 拼起的 JSON、`event:` 与下个 payload `type` 重复（应丢弃）、`custom_field` 未知字段、`model`/`id` 中途变化 | 注释透传；多行 data 拼成一个事件；重复的 event 行丢弃；`custom_field=demo` 显示；model/id 变化时重新显示 |
| `12-tricky-text.txt` | 难搞的文本：空 delta `''`、纯空格、`\n`、`\t`、引号、反斜杠、emoji、中文 | 全部 quoted & escaped，每 chunk 一行：`''`、`'   '`、`'\n'`、`'👸🎉'`、`'中文测试'` |
| `13-http-error.txt` | 非 SSE 的 HTTP 错误体（整段 JSON，无 `data:` 前缀） | 交给 `jq` 格式化；没装 jq 时原样透传 |
| `14-multi-choice.txt` | `n=2`：choices 数组里两个 index | 两个 choice 的 delta 分别渲染 |
| `15-non-stream.txt` | `stream: false` 的非流式响应：一整个 pretty-printed `chat.completion` JSON | 交给 `jq`：终端里 pretty + 着色，管道里压成一行；没装 jq 时原样透传 |

## 备注

- 所有 `data:` 行的 JSON 都是合法 JSON（多行 `data:` 的 11 号文件除外——那正是要测的 framing case，拼起来后合法）。
- 时间戳/ID 均为虚构；`11-sse-framing.txt` 里的 `custom_field` 是故意加的未知字段。
- legacy 的 `finish_reason=function_call` 未单独建文件（老式 function calling 已被 tool_calls 取代，渲染逻辑与 tool_calls 一致）。
