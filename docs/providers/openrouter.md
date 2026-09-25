# OpenRouter Integration Guide

OpenRouter expands VT Code with access to the full model marketplace. This guide covers configuration, CLI usage, and tips for
working with custom OpenRouter model IDs.

## Prerequisites

1. [Create an OpenRouter account](https://openrouter.ai) and generate an API key.
2. Export the API key in your shell or add it to a local `.env` file:

```bash
export OPENROUTER_API_KEY="your-openrouter-key"
# or
cat <<'ENV' > .env
OPENROUTER_API_KEY=your-openrouter-key
ENV
```

## Quickstart

Run VT Code against the Xiaomi MiMo V2.6 Pro model (the default OpenRouter model):

```bash
vtcode --provider openrouter --model xiaomi/mimo-v2.6-pro chat
```

Or use the efficient MiMo V2.6 Flash:

```bash
vtcode --provider openrouter --model xiaomi/mimo-v2.6-flash chat
```

Or use the fastest MiMo V2.6 Pro UltraSpeed:

```bash
vtcode --provider openrouter --model xiaomi/mimo-v2.6-pro-ultraspeed chat
```

Both commands stream responses using the OpenAI-compatible Chat Completions API and support VT Code tooling out of the box.

## Meta Muse models

For direct Meta access, prefer the [official Meta AI provider](./meta.md).
OpenRouter also exposes these marketplace model IDs:

```bash
vtcode --provider openrouter --model meta/muse-spark-1.2 chat
vtcode --provider openrouter --model meta/muse-glimmer-30b chat
```

These routes use `OPENROUTER_API_KEY` and OpenRouter billing. The `meta/...`
namespace is distinct from the official provider's bare `muse-spark-1.2` and
`muse-spark-1.1` IDs.

## Persisting configuration

Add OpenRouter to your workspace `vtcode.toml` (MiMo V2.6 Pro is the default):

```toml
[agent]
provider = "openrouter"
default_model = "xiaomi/mimo-v2.6-pro"
```

Custom model IDs are accepted. If you reference a model not listed in `docs/models.json`, ensure it is enabled for your
OpenRouter account.

## Runtime behaviour

- **Tool calling:** VT Code maps OpenRouter conversations to the OpenAI-compatible function calling format.
- **Streaming:** Streaming is fully supported for OpenRouter providers (VT Code uses the standard streaming interface).
- **Prompt refinement:** The prompt refiner automatically reuses your OpenRouter key and respects any custom model overrides.
- **Routing:** When the LLM router is enabled, VT Code honours the configured provider and model combination for routing tasks.

## Troubleshooting

| Symptom | Resolution |
| --- | --- |
| `HTTP 403` or `401` errors | Confirm `OPENROUTER_API_KEY` is set and active for the chosen model. |
| Model not found | Double-check the model slug in the [OpenRouter catalog](https://openrouter.ai/docs/llms) and your workspace config. |
| Tool calls ignored | Ensure the model you selected advertises tool support. Many third-party providers expose read-only models. |

For additional details, consult the [OpenRouter API reference](https://openrouter.ai/docs/api-reference/overview/llms) and the
[streaming documentation](https://openrouter.ai/docs/api-reference/streaming/llms).
