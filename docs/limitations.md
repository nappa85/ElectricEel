# Limitations

Facts about current behavior. Items are removed when fixed, not tracked
historically (see `git log` for history).

- Navigation share requires a real-car pass per firmware: the BLE
  encoding is byte-exact tested against the documented schema, but only
  the car proves a firmware version honors field-53/field-21 actions.
  Fallback: field-90 waypoints with pasted `refId:` Place IDs
  (`NavigateToWaypoints` is implemented, unwired). See
  `navigation-share.md`.
- Share reception requires the app running when the share arrives;
  Sailfish Share does not launch a closed app. The in-app Navigation
  page with copy-paste always works regardless.
- Android sharesheets cannot list the app (Jolla hardcodes that bridge
  for its own apps). From Android apps: copy, then paste into the
  Navigation page.
- `CommandCatalog.js` argument definitions are shared mutable
  singletons; the dialog overwrites `__value` on open. Cloned defs always
  drop the previous `__value` before applying state defaults, so stale
  values cannot survive across dialogs.
- `CommandCatalog.js` argument bounds and enum values are best-effort
  from public docs; `tesla-control` itself is the final authority and
  rejects anything it dislikes.
- QML files are not compiled by `mb2`; runtime QML errors remain
  possible despite a clean C++/Qt build.
- Only QML/JS user strings are translated; Rust/C++ diagnostics stay
  English. See `translations.md`.
