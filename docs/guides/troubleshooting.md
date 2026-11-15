# Troubleshooting

## "the site API refused this address (HTTP 403)"

The room lookup is behind Cloudflare and some addresses (datacentres, some VPN
exits, and some countries are age-gated) are refused. Set a proxy in Settings, for example a local Tor:

```sh
brew install tor && brew services start tor
# then in Settings: socks5h://127.0.0.1:9050
```

Media segments always go direct, so the proxy only carries the small lookups.

## "age-gate-required"

Chaturbate decides the age gate by exit country, not by account. A proxy moves
the lookup; the app rotates the circuit when the gate trips.

## ffmpeg missing

Health shows `ffmpeg missing` and recordings are not remuxed. Install ffmpeg or
point Settings at the binary. Everything already recorded still plays.

## A recording ended early

If the room went offline, that is expected. If it stops while the room is live,
check the job message in the queue: it names the failure. Interrupted files are
kept, never deleted.

## The output file is a strange length

A `.recording.mp4` that was killed mid-write keeps correct content but may
report odd duration metadata. Remux it (`convert --preset copy`) and the
duration is right.

## Nothing is recorded but the room is live

Check Health: the room API needs cookies, which the app harvests from the
homepage. A gated exit answers HTML instead, and the app says so.
