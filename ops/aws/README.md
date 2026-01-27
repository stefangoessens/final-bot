# AWS Deployment (SSM-first)

Hard rule: **do not connect to Polymarket from the local machine**. Use AWS for any Polymarket connectivity.

This directory provides a simple, repeatable workflow to:
- provision an EC2 canary instance (Amazon Linux 2023)
- install/build the bot
- run it under `systemd`
- update/redeploy via SSM (no SSH required)

## Prereqs

- AWS CLI configured (`default` profile is fine)
- Region: `eu-west-1` (Dublin)
- Instance profile with SSM enabled (e.g. `AmazonSSMManagedInstanceCore`)
- `Session Manager` plugin installed if you want interactive `start-session` (optional; scripts use `send-command`)

## One-command deploy (canary)

From repo root:

```bash
./ops/aws/pmmm_aws.sh deploy
```

This will:
- create an SSM-only security group (no inbound)
- launch a new instance (or reuse an existing running one with the same Name tag)
- install deps, clone `main`, build `--release`, install to `/usr/local/bin/polymarket-mm-bot`
- create `/etc/pmmm/pmmm.env` with `PMMB_TRADING__DRY_RUN=true`
- start `pmmm-bot.service`

## Status / logs

```bash
./ops/aws/pmmm_aws.sh status <instance-id>
./ops/aws/pmmm_aws.sh health <instance-id>
./ops/aws/pmmm_aws.sh logs <instance-id>
```

## Update (redeploy `main`)

After pushing to `main`:

```bash
./ops/aws/pmmm_aws.sh update <instance-id>
```

This will `git fetch/reset`, rebuild, replace the binary, and restart the systemd service.

## Config / secrets (AWS host only)

The bot reads config from:
- env vars prefixed with `PMMB_` (systemd loads `/etc/pmmm/pmmm.env`)
- optional config file via `PMMB_CONFIG_PATH` (TOML/JSON)

**Do not put secrets in git. Do not paste secrets into the local machine shell history.**

For now, edit `/etc/pmmm/pmmm.env` on the instance (via SSM interactive session) and restart:

```bash
aws ssm start-session --region eu-west-1 --target <instance-id>
sudoedit /etc/pmmm/pmmm.env
sudo systemctl restart pmmm-bot.service
```

Notes:
- Keeping `PMMB_TRADING__DRY_RUN=true` will prevent posting orders, but the bot may still need API creds to connect to Polymarket data feeds.
- Enabling live trading must comply with repo rule: **do not bypass geoblocks**.

## First-time bring-up checklist (common “stale feeds” issue)

If `./ops/aws/pmmm_aws.sh health <instance-id>` shows:
- `ws_market_connected 0` / `rtds_connected 0`
- feed `status: stale`

then you likely haven’t provided Polymarket API credentials on the instance yet.

On the AWS instance, set (at minimum) the CLOB API creds and restart:

```bash
aws ssm start-session --region eu-west-1 --target <instance-id>
sudoedit /etc/pmmm/pmmm.env
sudo systemctl restart pmmm-bot.service
```

Expected success signal:
- `/healthz` shows feeds as `ok` (not `stale`)
- metrics include `ws_market_connected 1` and `rtds_connected 1` (after warm-up)

## “restricted market …” / quoting halted

If `/healthz` reports `halted_reason` or `quoting_block_reason` like `restricted market ...`, the bot is intentionally refusing to quote.

Action:
- Treat this as a compliance/safety block and investigate the underlying API response (via logs).
- Do **not** patch around it; the repo rule is explicit: **do not bypass geoblocks**.

## Safe defaults / hardening

- The deploy script creates an SG with **no inbound rules** (SSM-only).
- Health/metrics bind to `0.0.0.0` by default; keep SG inbound closed unless you intentionally expose them.
