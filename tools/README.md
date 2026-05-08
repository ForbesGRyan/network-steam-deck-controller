# tools/

Reference materials and helper scripts. The contents of `tools/reference/`
are **not committed** — fetch them when you need them.

## reference/

The Linux kernel's `hid-steam` driver and the HID-ID header are kept here
as historical context for the Steam Deck controller's HID protocol. They
were load-bearing for the original custom-driver design (now deleted; see
`git log -- driver/`). The current `usbip` backend tunnels HID URBs raw,
so this code is no longer referenced from any crate — keep these files
around only if you're investigating Deck input behaviour at the protocol
level.

Fetch:

```sh
mkdir -p tools/reference
curl -sSL https://raw.githubusercontent.com/torvalds/linux/master/drivers/hid/hid-steam.c \
    -o tools/reference/hid-steam.c
curl -sSL https://raw.githubusercontent.com/torvalds/linux/master/drivers/hid/hid-ids.h \
    -o tools/reference/hid-ids.h
```

These are GPL-2.0+; keeping them out of the repo preserves the workspace's
MIT/Apache-2.0 licensing.
