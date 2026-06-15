# Supervising the monitor (`a3s-box monitor`)

`a3s-box` is daemonless: a detached box (`run -d`) is supervised by a separate
`a3s-box monitor` process that polls `boxes.json`, restarts dead boxes per their
`--restart` policy, and acts on health checks (with exponential backoff).

On its own the monitor is just a foreground process. **If it isn't running,
detached boxes silently lose restart + health supervision** — nothing restarts a
crashed box, and `status` can stay `starting`. Running it by hand
(`a3s-box monitor &`) works until that shell or process dies.

## Install it as a supervised service (recommended)

```bash
a3s-box monitor --install            # default 5s poll
a3s-box monitor --install --interval 10
```

This installs and enables a **per-user** service — no root required:

| OS | Mechanism | Unit |
|----|-----------|------|
| Linux | systemd `--user` | `~/.config/systemd/user/a3s-box-monitor.service` |
| macOS | launchd LaunchAgent | `~/Library/LaunchAgents/com.a3s-box.monitor.plist` |

The OS keeps the monitor running and restarts it if it crashes
(`Restart=always` / `KeepAlive`). The unit's `ExecStart` points at the absolute
path of the `a3s-box` binary you ran `--install` with.

### Linux: headless hosts

`systemctl --user` services stop when your login session ends. For a server that
should supervise boxes without an active login, enable lingering once:

```bash
loginctl enable-linger "$USER"
```

### Remove it

```bash
a3s-box monitor --uninstall
```

If automatic enable/load fails (e.g. systemd/launchctl unavailable in your
environment), `--install` still writes the unit file and prints the manual
`systemctl --user enable --now …` / `launchctl load -w …` command to finish.
