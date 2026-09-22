> [!IMPORTANT]
> LLM disclosure: This codebase was written with substantial help from large language models: AI coding agents working from the [`AGENTS.md`](AGENTS.md) brief in this repo.

This is a fork of [github.com/wimpysworld/ia-get](https://github.com/wimpysworld/ia-get) that adds support for restricted items.

To authenticate with your archive.org account:

```shell
ia-get --username <email> --password "<password>" https://archive.org/details/<identifier>
```

You can avoid putting the password in shell history by reading it from standard input:

```shell
printf '%s' "$IA_GET_PASSWORD" | ia-get --username <email> --password-stdin https://archive.org/details/<identifier>
```