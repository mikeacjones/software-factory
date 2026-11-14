# Stream fixtures

Output of `claude -p --output-format stream-json --verbose`, one JSON object per line.

- `success-haiku-2.1.325.jsonl` was recorded from CLI 2.1.325 on 2026-11-14: a Haiku run that read a file, ran a tool and returned a typed result. Tool lists, tool output, thinking text and local paths were removed; every event keeps its shape.
- `limit-rejected-reconstructed.jsonl` was **not** recorded. It is rebuilt from the owner's notes of 2026-11-13 on a run refused at the usage limit: exit code 1, a result with `is_error` true but `subtype` "success", and a `rate_limit_event` with status `rejected`. Replace it with a recording when one is captured.
