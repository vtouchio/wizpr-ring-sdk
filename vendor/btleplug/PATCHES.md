# Local patches

This vendored copy is based on `btleplug` 0.11.8.

Applied changes:

- Increase the WinRT notification broadcast channel capacity from 16 to 4096.
  WIZPR Ring audio is delivered as frequent BLE notifications, and the default
  upstream buffer can drop notifications when the Windows receiver briefly lags.
- Restrict WinRT subscription descriptor selection to Notify only. WIZPR Ring
  audio and event characteristics use Notify semantics; Indicate is intentionally
  not used as a fallback for this product-specific vendored backend.
- Fix two existing compiler warnings in the vendored source so repository CI
  stays clean when the patch is built locally.

This should be revisited once the Windows audio path is validated. If the
larger buffer and Notify-only subscription fix the issue, upstreaming a
configurable notification buffer and product-level subscription policy, or
moving to a transport implementation with explicit backpressure, should be
considered.
