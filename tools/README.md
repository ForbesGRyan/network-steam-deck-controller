# tools/

Reference materials and helper scripts. The contents of `tools/reference/`
are **not committed** — fetch them when you need them.

## reference/

The Linux kernel's `hid-steam` driver and the HID-ID header are the canonical
source for the Steam Deck controller HID protocol. They are GPL-2.0+ and are
kept out of this repo so the workspace's MIT/Apache-2.0 licensing stays clean.

Fetch:

```sh
mkdir -p tools/reference
curl -sSL https://raw.githubusercontent.com/torvalds/linux/master/drivers/hid/hid-steam.c \
    -o tools/reference/hid-steam.c
curl -sSL https://raw.githubusercontent.com/torvalds/linux/master/drivers/hid/hid-ids.h \
    -o tools/reference/hid-ids.h
```

Where these are referenced in our code:

- `crates/deck-protocol/src/hid.rs` — `BUTTON_MAP` and report layout were lifted
  from `steam_do_deck_input_event` in `hid-steam.c`. Re-grep that function any
  time you suspect a bit-position bug.
- `crates/deck-protocol/src/buttons.rs` — flag names mirror the kernel
  driver's `BTN_*` mapping.
