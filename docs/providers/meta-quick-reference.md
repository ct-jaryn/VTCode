# Meta AI Quick Reference

| Setting           | Value                             |
| ----------------- | --------------------------------- |
| Provider          | `meta`                            |
| API key           | `META_API_KEY` or `MODEL_API_KEY` |
| Default endpoint  | `https://api.meta.ai/v1`          |
| Endpoint override | `META_BASE_URL`                   |
| Default model     | `muse-spark-1.2`                  |
| Context metadata  | 1,048,576 tokens                  |

```bash
export MODEL_API_KEY="..."
vtcode --provider meta --model muse-spark-1.2 ask "Summarize this change"
```

Official Meta Muse models:

- `muse-spark-1.2`
- `muse-spark-1.1`
- `muse-spark-1.2-contributor` (opt-in Contributor tier)

See the [full Meta AI provider guide](./meta.md).
