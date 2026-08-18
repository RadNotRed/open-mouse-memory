# HID++ transaction notes

## Reports

Standard HID++ short reports use report ID `0x10` and contain three parameter bytes. Long reports use report
ID `0x11` and contain sixteen parameter bytes.

```text
byte 0  report ID
byte 1  receiver device index
byte 2  runtime feature index
byte 3  function in the high nibble, software ID in the low nibble
byte 4+ parameters
```

Feature IDs such as `0x2201` and `0x8100` are resolved at runtime with `ROOT.GetFeature`. They are not wire
feature indexes.

HID++ 2.x errors return feature index `0xFF`, then echo the requested feature index and function/software-ID
byte, followed by the error code.

## Implemented feature calls

| Feature | Function | Direction | Meaning |
| --- | ---: | --- | --- |
| `0x0000 ROOT` | `0x00` | read | Resolve feature ID |
| `0x0000 ROOT` | `0x10` | read | Protocol version |
| `0x0001 FEATURE_SET` | `0x00` | read | Feature count |
| `0x0001 FEATURE_SET` | `0x10` | read | Feature ID/type/version by index |
| `0x0003 DEVICE_FW_VERSION` | `0x00`, `0x10` | read | Identity and firmware records |
| `0x0005 DEVICE_NAME` | `0x00`, `0x10` | read | Length and name fragments |
| `0x1000`, `0x1001`, `0x1004` | varies | read | Battery state |
| `0x2201 ADJUSTABLE_DPI` | `0x10`, `0x20`, `0x30` | read/read/write | DPI list, state, live set |
| `0x2202 EXTENDED_ADJUSTABLE_DPI` | `0x10`, `0x20`, `0x50`, `0x60` | read/read/read/write | X/Y/LOD capabilities and state |
| `0x8060 REPORT_RATE` | `0x00`, `0x10`, `0x20` | read/read/write | Millisecond report rates |
| `0x8061 EXTENDED_ADJUSTABLE_REPORT_RATE` | `0x10`, `0x20`, `0x30` | read/read/write | 125 Hz through 8 kHz rates |
| `0x8100 ONBOARD_PROFILES` | `0x00`, `0x20`, `0x40` | read | Descriptor, mode, active sector |
| `0x8100 ONBOARD_PROFILES` | `0x10` | write | Onboard/host mode |

The implementation preserves each discovered feature version so handlers can become version-aware when
capture evidence demonstrates a semantic difference.
