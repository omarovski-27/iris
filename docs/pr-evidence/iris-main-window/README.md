# Settings-window PR review evidence

Screenshots of the `iris-app` settings window (History, Settings, Insights),
rendered by a real `x86_64-pc-windows-gnu` build run as
`iris.exe --demo-window` — seeded config and session log under the system temp
dir — and captured through WSL/Windows interop for review of the
`fm/iris-main-window` PR. Captured at the commit that added them
(`docs(iris-app): add settings-window screenshots for PR review`, named rather
than pinned by hash, which rebasing invalidates); later commits on that branch
changed window details, so treat them as point-in-time evidence, not a current
rendering.

They are deliberately **not** linked from `crates/iris-app/README.md` or any
other product documentation. These are one-off review artifacts captured
against demo data on a single run; linking them would put unmaintained
illustrations in user-facing docs that go stale the next time the window's
design moves.
