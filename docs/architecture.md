# Architecture

The runtime has three strict layers:

```text
CLI and GUI actions
    ↓
Logical device and HID++ feature handlers
    ↓
HID++ transaction engine and HID transport
    ↓
hidraw
```

The CLI and GUI never open or write a HID endpoint directly. Device handlers never hardcode runtime feature
indexes. The transaction layer owns framing, software IDs, stale-input draining, response correlation,
timeouts, read-only retries, protocol errors, and raw tracing. Read calls retry once after a timeout with a
new software ID. Writes are never retried automatically because a lost acknowledgement does not prove that a
device ignored the mutation.

## Discovery

1. Enumerate every Logitech HID interface and retain its path, VID/PID, serial, strings, usage, interface,
   and bus.
2. Prioritize interface 2 and vendor usage pages as likely HID++ endpoints.
3. Probe receiver device index `0x01` for known one-device LIGHTSPEED receivers, indexes `0x01` through
   `0x06` for generic receivers, and `0xFF` for direct devices.
4. Use `ROOT.GetProtocolVersion` to accept only HID++ 2.x or newer devices.
5. Resolve `FEATURE_SET`, enumerate every runtime feature, and retain feature ID, index, type, and version.
6. Read `DEVICE_NAME` through the feature table.

The USB receiver PID identifies the physical transport, not the mouse. A logical identity always includes
the receiver path and device index.

## Writes

The v0.1 write surface is deliberately small:

- live DPI through `0x2201` or `0x2202`;
- live report rate through `0x8060` or `0x8061`;
- onboard/host mode through `0x8100` function `0x10`;
- decoded format-7 profile fields through typed `0x8100` sector writes.

Each handler validates, writes, reads the state back, and compares it to the requested value. Live handlers
do not silently disable onboard mode. The format-7 profile handler reads the current sector, changes only
decoded fields, updates its checksum, writes it, and verifies the complete readback.

The GUI loads the five onboard slots. DPI stages, report rate, and standard mouse or DPI button actions are
stored on the device. Unknown button records and undecoded bytes are preserved. Custom keys and macros remain
outside the current write surface.

## Testability

`HidIo` is the transport boundary. The real implementation wraps `hidapi::HidDevice`; tests use a queued
fake implementation. Protocol parsing remains independent of both the CLI and hardware.
