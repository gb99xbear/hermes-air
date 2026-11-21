# Hermes Air parity run - 2026-11-17

Method: identical 14-prompt fixed set (`parity/prompts.json`) sent to BOTH
implementations in one-shot mode, same model
`river/deepseek-ai/DeepSeek-V4.1-Flash-1017` via 9router.

- Python Hermes: `hermes -z "<prompt>" -m <model>` (Tencent, gateway host)
- Hermes Air:    `hermes-gateway -c config.toml -m <model> -q "<prompt>"` (Oracle)

Each run is one turn; metrics come from each side's own parity JSONL.

## Per-prompt

| prompt | api py/air | tools py/air | prompt_tok py/air | out py/air | lat_ms py/air | struct |
|---|---|---|---|---|---|---|
| p01_text_simple | 1/1 | 0/0 | 117883/16459 | 6/5 | 3819/1401 | OK |
| p02_text_math | 1/1 | 0/0 | 118075/16460 | 3/2 | 2814/1203 | OK |
| p03_tool_read | 3/2 | 2/1 | 354854/33004 | 355/90 | 10187/4761 | DIF |
| p04_tool_terminal | 2/2 | 1/1 | 237047/32992 | 132/62 | 4902/3595 | OK |
| p05_tool_write | 2/2 | 1/1 | 236056/33048 | 273/103 | 6831/2385 | OK |
| p06_tool_multi | 2/2 | 1/2 | 236526/33048 | 101/150 | 5313/3407 | DIF |
| p07_tool_error | 3/2 | 2/1 | 354302/33022 | 476/169 | 10224/3511 | DIF |
| p08_tool_search | 4/2 | 4/1 | 474187/33038 | 931/94 | 17211/2767 | DIF |
| p09_unicode | 1/1 | 0/0 | 118046/16454 | 5/4 | 3506/978 | OK |
| p10_long_context | 1/1 | 0/0 | 130674/28862 | 383/290 | 7220/4075 | OK |
| p11_slash_model | 6/0 | 8/0 | 803401/0 | 2627/0 | 35437/0 | DIF* |
| p12_slash_status | 3/0 | 3/0 | 356541/0 | 1278/0 | 22085/0 | DIF* |
| p13_multi_turn | 1/1 | 0/0 | 117897/16474 | 131/2 | 4255/1676 | OK |
| p14_compression | 1/1 | 0/0 | 132601/30878 | 7/6 | 2954/1789 | OK |

Structural match (api_calls + tool_calls + finish_reason): **8/14**

## Totals

| metric | Python | Air | air/py |
|---|---|---|---|
| api_calls | 31 | 18 | 0.58 |
| tool_calls | 22 | 7 | 0.32 |
| prompt_tokens | 3,788,090 | 323,739 | 0.09 |
| output_tokens | 6,708 | 977 | 0.15 |
| latency_ms | 136,758 | 31,548 | 0.23 |
| mean latency | 9,768 ms | 2,253 ms | - |

## Findings

1. **Efficiency: Air is far lighter.** ~9% of the prompt tokens, ~23% of the
   latency. Most of the token gap is expected: Python injects skills, memory and
   a large system prompt; Air does not (that is the point of the lightweight
   rewrite). Token parity is therefore NOT the right gate.

2. **Behavioural parity not yet reached on tool-heavy prompts.** p03/p07/p08:
   Python runs more API rounds and more tool calls than Air for the same ask.
   Air still reaches `finish_reason=stop` but with a shorter tool loop. This is
   the real gap to close if parity is the goal.

3. **p11/p12 are a methodology artifact, not a divergence.** `hermes -z "/model"`
   sends the text as a normal prompt; Air's `-q "/model"` parses it as a slash
   command (0 API calls). One-shot is the wrong harness for slash commands -
   compare those through an interactive/REPL surface instead.

4. p06 is the one case where Air used MORE tool calls than Python (2 vs 1) -
   so the tool-loop difference is not uniformly "Air does less".

## Caveats

- p10/p14 use a 40k-char fixture (a 137k-char arg exceeds the OS 128 KiB
  per-argument limit); both sides used the identical trimmed fixture.
- One run per prompt: single-sample latency, not a distribution.

## Artifacts

- `parity/prompts.json`, `parity/long_fixture.txt`
- `parity/air_results.jsonl`, `py_results.jsonl` (copied alongside)
