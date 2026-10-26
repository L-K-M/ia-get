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

## License

ia-get is a fork of [wimpysworld/ia-get](https://github.com/wimpysworld/ia-get). Its code, including this fork's changes, is released under the [MIT License](LICENSE) (Copyright (c) 2023 Martin Wimpress et al.).

The release binaries statically link Rust crates and the Rust standard library, which keep their own licenses. Their notices ship as `Third-Party-Notices.txt` and `Rust-Licenses.html` in each release archive and in `/usr/share/doc/ia-get/` (`/usr/share/doc/ia-get-musl/` for the musl build) of the .deb packages. The crate colored is used unmodified under the MPL-2.0; `Third-Party-Notices.txt` gives its version and the location of its source.
