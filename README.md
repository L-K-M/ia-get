This is a fork of [github.com/wimpysworld/ia-get](https://github.com/wimpysworld/ia-get) that adds support for restricted items.

To authenticate with your archive.org account:

```shell
ia-get --username <email-or-username> --password "<password>" https://archive.org/details/<identifier>
```

You can avoid putting the password in shell history by reading it from standard input:

```shell
printf '%s' "$IA_GET_PASSWORD" | ia-get --username <email-or-username> --password-stdin https://archive.org/details/<identifier>
```