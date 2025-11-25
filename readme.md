This connects two dbus sockets together, forwarding messages from one to the other (i.e. acting as a server on one, client on the other). This is unlike xdg-dbus-proxy which connects to one socket and _creates_ another socket.

You can use this to expose services from one dbus on another dbus, when you can't launch multiple instances. For instance, you only want one notification daemon, but want it to display notifications from multiple sessions.

I use this in conjunction with `xdg-dbus-proxy` for containers, where I use the `proxy` to make a notification-only socket I mount into containers, then `dbusgraft` to reroute notification messages from the container bus to the proxied bus.

# Help

```
dbusgraft -h
Usage: /mnt/home-dev/.cargo_target/debug/dbusgraft ...FLAGS

    Connects to 2 dbus sockets and registers as a server for a set of services on the downstream, and acts as a client on the upstream.  Any requests it receives as a server it'll forward to the cooresponding server on the upstream side.

    --upstream <STRING>            The connection with the desired servers. As you'd see in `DBUS_SESSION_BUS_ADDRESS`, ex: `unix:path=/run/user/1910/bus`
    --downstream <STRING>          The connection with no servers, where this will pose as a server. Ex: `unix:path=/run/user/1910/bus`
    --register <STRING>[ ...]      The names to act as a server for/forward. Ex: `org.freedesktop.Notifications`

```

# Installation

1. Clone the repo
2. Do `cargo build`
3. The binary will be in `target/debug/dbusgraft`

# Common services

- Notifications

  Register `org.freedesktop.Notifications`. If using `xdg-dbus-proxy` you also need `--talk=org.freedesktop.Notifications`

- XDG Portal (Camera?)

  Register `org.freedesktop.portal.Desktop`. If using `xdg-dbus-proxy` you also need `--talk=` for each of them.

# Caveats

- Some dbus protocols (services? interfaces?) are badly abstracted and break when forwarded. For instance, the XDG Portal `AccessCamera` method returns a signal path that references connection-specific details in a non-standard way.

  I added a specific workaround for that case, but there may be other protocols that have similar details. I'm happy to add more workarounds if you can provide details and help with testing.

# Troubleshooting

The primary way to debug this is to use `dbus-monitor`. If you have an issue, try getting whatever you're doing working without `dbusgraft` and recording it with `dbus-monitor`. Then try it again with `dbusgraft` - this time run `dbus-monitor` on the upstream and downstream busses. Compare the results and find which messages are missing or have incorrect values in the second run.

There's also a debug flag which may increase `dbusgraft`'s logging.
