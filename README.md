# TermHog

Record terminal sessions and stream them to [PostHog](https://posthog.com) as
session replays.

TermHog runs CLI commands, outputs verbatim to the host terminal, and in the
background projects the terminal screen into rrweb events that PostHog's
session replay player understands.

## Library

```bash
cargo add termhog
```

`termhog::Command` mirrors `std::process::Command`, and the child behaves as if
std had spawned it. Streams left inherited are recorded on their way to the
terminal. Anything you redirect (a file, a pipe, null) is left alone.

```rust
use termhog::{Command, TermHog};

fn main() -> termhog::Result<()> {
    // Always first: see "Background uploads" below.
    termhog::init();

    let outcome = TermHog::new("phc_xxx")
        .distinct_id("user-123")
        .status(Command::new("npm").arg("test"))?;

    println!("exit: {}", outcome.status);
    println!("replay: {}", outcome.replay_url);
    Ok(())
}
```

`TermHog::spawn` returns a `Recording` handle (like `std::process::Child`) for
piped streams, `kill`, `wait` and `try_wait`. It's synchronous, with no async
runtime: call it from `spawn_blocking` in async code. Unix only for now.

### Background uploads

A slow network never holds up your program. Shortly after a command exits
(0.3 seconds, or 20 in CI), whatever isn't uploaded yet is saved to the user's
cache folder (`termhog/` in it, readable only by them), and your program's own
executable is started again, detached and silent, to finish the upload.
`termhog::init()` is what does that work in the relaunched process (and then
exits), so it must be the first thing in `main`: anything before it runs
again in the uploader. `spawn` panics if `init` wasn't called. Anything a
background upload can't finish (the machine shut down, say) is picked up by
the next `init`. In CI, where leftover processes don't outlive the job,
nothing is started in the background.

### Signals

While recording, signals reach the command as they would natively, with one
limit when it doesn't get its own terminal: a signal sent to your whole
process group reaches it twice. See the
[crate docs](https://docs.rs/termhog/latest/termhog/#signals) for details.

## CLI

```bash
cargo install termhog-cli
```

Put the command to record after `--`:

```bash
POSTHOG_API_KEY=phc_xxx termhog -- nvim
POSTHOG_API_KEY=phc_xxx termhog -- npm test
```

It exits with the command's own status (127 if the command wasn't found, 126 if
it couldn't be run) and prints a link to the replay, which may take a few
minutes to process within PostHog. Uploads a slow network doesn't finish in
time continue in the background (see [Background uploads](#background-uploads)).

Each setting is a flag, or the matching environment variable (an empty value
counts as unset). Flags go before the `--`:

- `--api-key` / `POSTHOG_API_KEY`: project token (the public `phc_` token). Required.
- `--host` / `POSTHOG_HOST`: ingestion host (default `https://us.i.posthog.com`). The known Cloud hosts auto-resolve their app host for replay links.
- `--ui-host` / `POSTHOG_UI_HOST`: replay-UI host, for ingestion behind a proxy. Defaults to the ingestion host.
- `--distinct-id` / `POSTHOG_DISTINCT_ID`: the person ID. Without it, recordings are anonymous, and PostHog creates no person profile for them.
- `--session-id` / `POSTHOG_SESSION_ID`: the session ID, so external events can share it. Defaults to a fresh UUIDv7.

When output isn't a terminal (CI logs, pipes), the recorded screen size comes
from `COLUMNS` and `LINES` if both are set, and is 80x24 otherwise.

## License

MIT
