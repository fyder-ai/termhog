# ph-capture

Record terminal sessions and stream them to [PostHog](https://posthog.com) as
session replays.

`ph-capture` wraps a command under a pty, mirrors its output to your terminal,
and in the background projects the terminal screen into rrweb events that
PostHog's replay player understands.

## Install

```bash
cargo install ph-capture
```

## Usage

Put the command to record after `--`:

```bash
# record an interactive editor
POSTHOG_API_KEY=phc_xxx ph-capture -- nvim

# record a test run
POSTHOG_API_KEY=phc_xxx ph-capture -- npm test
```

It passes the command's output through to your terminal, exits with the
command's own status, and prints a link to the session replay (which may take a
few minutes to process within PostHog).

## Configuration

All settings come from the environment:

- `POSTHOG_API_KEY` — project token (the public `phc_` token). Required.
- `POSTHOG_HOST` — ingestion host (default `https://us.i.posthog.com`). The known Cloud hosts auto-resolve their app host for replay links.
- `POSTHOG_UI_HOST` — replay-UI host, for ingestion behind a proxy. Defaults to the ingestion host.
- `PH_CAPTURE_DISTINCT_ID` — pin the person ID. Defaults to a stable anonymous ID persisted in the config dir.
- `PH_CAPTURE_SESSION_ID` — pin the session ID, so external events share it. Defaults to a fresh UUIDv7 per run.

The `POSTHOG_*` names match PostHog's own conventions. The `PH_CAPTURE_*` names
let an external orchestrator pin the person and session so related events line
up.

## License

MIT
