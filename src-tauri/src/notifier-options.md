# Toast click handling: 3 options

Background: toasts persist in the Action Center, but a click only opens the
stream if something receives the activation. All options below assume
`Long` duration + Action Center history (already in `notifier.rs`).

## 1. In-process callbacks (current)

`tauri-winrt-notification` directly: `on_activated` captures the login and
calls the opener from the COM callback. One worker thread serializes
`show()` calls; nothing ever waits.

- Clicks work for the entire process lifetime, including from history.
- Clicks die with the process. A leftover toast clicked after
  restart/exit goes nowhere (no COM activator registered).
- Cost: ~0. Already implemented.

## 2. Protocol activation (recommended if 1. isn't enough)

Put the already-registered `https` scheme to work — no custom scheme, no
registry writes, no app-side handling:

```xml
<toast launch="https://www.twitch.tv/pokimane" ...>
  <actions>
    <action content="Watch"
            arguments="https://www.twitch.tv/pokimane"
            activationType="protocol"/>
  </actions>
</toast>
```

Click (body, button, history) → the shell opens the browser, app running
or dead. All callback/worker-wait/opener code in `notifier.rs` deletes;
`show()` becomes pure fire-and-forget.

- Blocker: the fork hardcodes `<action content='' arguments=''/>` and has
  no `launch` support, so it needs a ~10-line XML patch (via
  `[patch.crates-io]`, or vendor the ~40 lines of toast building and drop
  the dep).
- Nothing else changes: no registry, no argv parsing, no lifecycle.

## 3. COM activator server

Implement `INotificationActivationCallback` in-process, register CLSID +
`LocalServer32` + `ToastActivatorCLSID` (all HKCU). Windows launches the
exe on click when closed, routes into it when running. Pure Rust via the
`windows` crate (`implement!`, `CoRegisterClassObject`, STA pump) —
~150 lines plus lifecycle footguns (don't exit before the callback lands,
don't linger after).

- Same outcome as 2 (restart-proof clicks) at 10x the complexity.
- Only advantage over 2: activation stays in-app (could focus window,
  route conditionally) instead of always opening the browser.
- Not recommended unless clicks must do more than open a URL.
