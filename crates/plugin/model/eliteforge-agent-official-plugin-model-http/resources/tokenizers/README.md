# Embedded model tokenizers

Resources are gzip-compressed without timestamp variation. SHA-256 values below
refer to uncompressed publisher files. License texts are kept beside each asset.
No model weights are included. GPT vocabularies come from the locked MIT-licensed
`tiktoken-rs` dependency (OpenAI's `cl100k_base` and `o200k_base` dictionaries).

| Asset | Publisher / pinned revision | License | SHA-256 |
| --- | --- | --- | --- |
| deepseek-v4.1-flash.tokenizer.json.gz | [deepseek-ai/DeepSeek-V4.1-Flash](https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash/tree/314113f8a7dadb388215396ba5fbee2c610a04d6), `314113f8a7dadb388215396ba5fbee2c610a04d6` | MIT (`deepseek-v4.1-flash.LICENSE`) | `c90dfa01249db1be4245780a052ede752e1361c612ac6d08e2bdada7d599476b` |
| glm-5.3-flash.tokenizer.json.gz | [zai-org/GLM-5.3-Flash](https://huggingface.co/zai-org/GLM-5.3-Flash), content pinned by hash | MIT (`GLM-5.3-Flash-LICENSE`) | `19e773648cb4e65de8660ea6365e10acca112d42a854923df93db4a6f333a82d` |
| qwen3.tokenizer.json.gz | [Qwen/Qwen3-8B](https://huggingface.co/Qwen/Qwen3-8B/tree/b968826d9c46dd6066d109eabc6255188de91218), `b968826d9c46dd6066d109eabc6255188de91218` | Apache-2.0 (`qwen3.LICENSE`) | `aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4` |
| qwen3.5.tokenizer.json.gz | [Qwen/Qwen3.5-9B](https://huggingface.co/Qwen/Qwen3.5-9B/tree/c202236235762e1c871ad0ccb60c8ee5ba337b9a), `c202236235762e1c871ad0ccb60c8ee5ba337b9a` | Apache-2.0 (`qwen3.5.LICENSE`) | `5f9e4d4901a92b997e463c1f46055088b6cca5ca61a6522d1b9f64c4bb81cb42` |
| kimi-k2.5.tiktoken.model.gz | [moonshotai/Kimi-K2.5](https://huggingface.co/moonshotai/Kimi-K2.5/tree/4d01dfe0332d63057c186e0b262165819efb6611), `4d01dfe0332d63057c186e0b262165819efb6611` | Modified MIT (`kimi-k2.5.LICENSE`) | `b6c497a7469b33ced9c38afb1ad6e47f03f5e5dc05f15930799210ec050c5103` |
| claude-legacy.json.gz | Anthropic official npm package `@anthropic-ai/tokenizer@0.0.4`, `claude.json` | MIT (`claude-legacy.LICENSE`) | `58dad83d85e9cd57be209172449ebfbc395df2b455c11fe8b7e5b661e6f462ad` |

The Anthropic tarball was verified against its published SHA-512 integrity:
`sha512-EHRKbxlxlc8W4KCBEseByJ7YwyYCmgu9OyN59H9+IYIGPoKv8tXyQXinkeGDI+cI8Tiuz9wk2jZb/kK7AyvL7g==`.
Its NFKC normalization and packed BPE rank offsets are preserved. Literal special
markers are counted as ordinary text; no conversation control tokens are added.
Anthropic explicitly says this legacy algorithm is only a very rough approximation
for Claude 3 and later: [official notice](https://github.com/anthropics/anthropic-tokenizer-typescript).
It is never labeled an exact current Opus tokenizer.

Kimi uses the pre-tokenization expression in the publisher's `tokenization_kimi.py`
at the pinned revision. Hugging Face resources disable padding and truncation for
counting. Default family rules share these representative dictionaries; exact model
rules and host registrations can override them. Text counts do not include server
chat templates, hidden gateway prompts, media processing, or protocol framing.
