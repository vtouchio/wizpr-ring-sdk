# Security Policy

## Supported Scope

This repository contains the WIZPR Ring application SDK and reference desktop validation tool. The supported security scope includes:

- SDK APIs for BLE discovery, connection lifecycle, audio streaming, events, and safe status requests.
- Protocol parsing, ADPCM decoding, and WAV writing code.
- Reference desktop CLI behavior shipped from this repository.
- Build, packaging, and dependency issues that affect this SDK.

This SDK is not a firmware control or manufacturing toolkit. It does not intentionally expose raw firmware commands, calibration commands, OTA/DFU controls, or factory diagnostics.

## Device Security Boundary

The SDK can reduce misuse by keeping firmware control surfaces out of public APIs, but it is not the only security boundary for a physical BLE device. Device authentication, command authorization, firmware update policy, and low-level BLE access control must be enforced by device firmware and product applications.

Do not assume that hiding an SDK method prevents all BLE-level access. General BLE tools and independent implementations may still interact with exposed device characteristics.

## Reporting a Vulnerability

Please do not open public GitHub issues for vulnerabilities.

Report suspected security issues through VTouch's official contact channel:

```text
contact@vtouch.io
```

The public website is available at <https://www.vtouch.io/> and includes a Contact section.

Include as much detail as possible:

- affected SDK version or commit
- platform and Bluetooth stack
- device model and firmware version, if known
- reproduction steps
- impact assessment
- logs or proof-of-concept details that are safe to share privately

We will acknowledge reports as soon as practical and coordinate follow-up based on severity and reproducibility.

## Public Disclosure

Please allow VTouch reasonable time to investigate and prepare a fix or mitigation before public disclosure. We prefer coordinated disclosure for issues that affect deployed devices, SDK users, or downstream applications.

## Out of Scope

The following are usually out of scope for this SDK repository unless they demonstrate a concrete SDK vulnerability:

- Generic BLE sniffing or writes that do not use this SDK.
- Physical attacks requiring device disassembly.
- Social engineering.
- Denial-of-service against a local Bluetooth adapter without SDK involvement.
- Reports based only on unavailable future features.
