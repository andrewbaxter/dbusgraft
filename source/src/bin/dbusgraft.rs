use {
    aargvark::{
        Aargvark,
        vark,
    },
    futures::{
        TryStreamExt,
        channel::oneshot,
        future::join_all,
    },
    loga::{
        DebugDisplay,
        Log,
        ResultContext,
        ea,
        fatal,
    },
    moka::future::{
        Cache,
        CacheBuilder,
    },
    std::{
        borrow::Cow,
        num::NonZero,
        sync::Arc,
        time::Duration,
    },
    tokio::{
        select,
        spawn,
        task::yield_now,
    },
    zbus::{
        Connection,
        DBusError,
        MessageStream,
        names::UniqueName,
    },
};

#[derive(Clone)]
struct ReplyTypeRemote {
    original_serial: NonZero<u32>,
    is_access_camera: bool,
}

#[derive(Clone)]
enum ReplyType {
    //. TODO subscribe to name changes, stop when evicted
    //. LocalNameChanged,
    Remote(ReplyTypeRemote),
}

/// Connects to 2 dbus sockets and registers as a server for a set of services on
/// the downstream, and acts as a client on the upstream.  Any requests it receives
/// as a server it'll forward to the cooresponding server on the upstream side.
#[derive(Aargvark)]
struct Args {
    /// The connection with the desired servers. As you'd see in
    /// `DBUS_SESSION_BUS_ADDRESS`, ex: `unix:path=/run/user/1910/bus`
    #[vark(flag = "--upstream")]
    upstream: String,
    /// The connection with no servers, where this will pose as a server. Ex:
    /// `unix:path=/run/user/1910/bus`
    #[vark(flag = "--downstream")]
    downstream: String,
    /// The space separated names to act as a server for/forward. Ex:
    /// `org.freedesktop.Notifications`
    #[vark(flag = "--register")]
    register: Vec<String>,
    /// Enable verbose logging
    debug: Option<()>,
}

fn hack_replace_portal_path(s: &str, upstream_conn: &Connection, client: &UniqueName) -> String {
    // Hack:
    //
    // ```
    // signal time=1764009845.147693 sender=:1.48 -> destination=:1.51 serial=33 path=/org/freedesktop/portal/desktop/request/1_116/capture455715896; interface=org.freedesktop.portal.Request; member=Response
    // ```
    //
    // Instead of just responding, using polling, using an additional request to set
    // up a common signal path, buffering received signals, making do with static
    // signal handlers, or any number of well abstracted approaches, the protocal did
    // the intelligent thing and instead made the client and server both independently
    // come up with identical paths using internal dbus parameters to avoid a race
    // condition when registering the signal handler. Or at least that's how firefox
    // does it. As expected, leaky abstractions cause problems.
    let pat = upstream_conn.unique_name().unwrap()[1..].replace(".", "_");
    let replace = client[1..].replace(".", "_");
    let done = s.replace(&pat, &replace);
    return done;
}

async fn handle_upstream(
    log: loga::Log,
    downstream_conn: Connection,
    upstream_conn: Connection,
    client: UniqueName<'static>,
    reply_serial_lookup: Cache<NonZero<u32>, ReplyType>,
) -> Result<(), loga::Error> {
    let mut source_rx = MessageStream::from(&upstream_conn);
    loop {
        let Some(msg) = source_rx.try_next().await.context("Error receiving message from upstream")? else {
            break;
        };
        let header = msg.header();
        if header.member().map(|x| x.as_str()) == Some("NameAcquired") {
            // https://github.com/dbus2/zbus/issues/1318
            continue;
        }
        let body0 = msg.body();
        let mut body = Cow::Borrowed(body0.data().bytes());
        let mut out = zbus::message::Builder::from(header.clone());

        // Replace dest with original client
        out = out.destination(client.clone()).unwrap();

        // Replace reply with original serial
        if let Some(reply_serial) = header.reply_serial() {
            let Some(reply_info) = reply_serial_lookup.get(&reply_serial).await else {
                log.log_with(
                    loga::WARN,
                    format!("Couldn't find destination request for reply to [{}], ignoring", reply_serial),
                    ea!(msg = msg.dbg_str(), body = String::from_utf8_lossy(&msg.data())),
                );
                continue;
            };
            match reply_info {
                //. ReplyType::LocalNameChanged => {
                //.     continue;
                //. },
                ReplyType::Remote(remote_info) => {
                    out = out.reply_serial(Some(remote_info.original_serial));

                    // See Hack note below
                    'nohack: {
                        if remote_info.is_access_camera {
                            let Ok(v) = body0.deserialize::<zbus::zvariant::ObjectPath>() else {
                                break 'nohack;
                            };
                            body =
                                Cow::Owned(
                                    zbus::zvariant::to_bytes_for_signature(
                                        msg.data().context(),
                                        body0.signature(),
                                        &zbus::zvariant::ObjectPath::from_string_unchecked(
                                            hack_replace_portal_path(&v, &upstream_conn, &client),
                                        ),
                                    )
                                        .unwrap()
                                        .to_vec(),
                                );
                        }
                    }
                },
            }
        } else {
            'nohack: {
                let Some(((iface, member), path)) =
                    header.interface().zip(header.member()).zip(header.path()) else {
                        break 'nohack;
                    };
                if iface != "org.freedesktop.portal.Request" {
                    break 'nohack;
                }
                if member != "Response" {
                    break 'nohack;
                }
                out = out.path(hack_replace_portal_path(&path, &upstream_conn, &client)).unwrap();
            }
        }

        // Guarantee unique serial
        out = out.serial(zbus::message::PrimaryHeader::new(zbus::message::Type::MethodCall, 100).serial_num());

        // Replace sender with self
        if let Some(x) = downstream_conn.unique_name() {
            out = out.sender(x).unwrap();
        }

        // Build the message and send
        let msg = unsafe {
            out.build_raw_body(&body, header.signature().clone(), msg.data().fds().iter().map(|x| match x {
                zbus::zvariant::Fd::Borrowed(_borrowed_fd) => panic!(),
                zbus::zvariant::Fd::Owned(owned_fd) => zbus::zvariant::OwnedFd::from(owned_fd.try_clone().unwrap()),
            }).collect()).unwrap()
        };
        log.log_with(
            loga::DEBUG,
            "Sending message",
            ea!(msg = msg.dbg_str(), body = String::from_utf8_lossy(&msg.data())),
        );
        downstream_conn.send(&msg).await.context("Error forwarding reply")?;
    }
    return Ok(()) as Result<(), loga::Error>;
}

async fn handle_downstream(
    log: &loga::Log,
    upstream_socket: String,
    service: String,
    downstream_conn: Connection,
) -> Result<(), loga::Error> {
    let this_log = log.fork(ea!(graft = service, side = "down->up"));
    let reply_serial_lookup = CacheBuilder::default().time_to_idle(Duration::from_secs(60 * 10)).build();
    let serverside_conns = CacheBuilder::default().time_to_idle(Duration::from_secs(60 * 10)).build();
    let mut source_rx = MessageStream::from(&downstream_conn);
    let server_lookup = CacheBuilder::default().time_to_idle(Duration::from_secs(5)).build();
    'next_msg : loop {
        let Some(msg) = source_rx.try_next().await.context("Error receiving next message")? else {
            break;
        };
        let header = msg.header();
        if header.member().map(|x| x.as_str()) == Some("NameAcquired") {
            // https://github.com/dbus2/zbus/issues/1318
            continue;
        }
        let Some(client) = header.sender() else {
            this_log.log_with(
                loga::WARN,
                "Received sender-less message",
                ea!(msg = msg.dbg_str(), body = String::from_utf8_lossy(&msg.data())),
            );
            continue;
        };
        let serverside_conn;
        match serverside_conns.try_get_with::<_, loga::Error>(client.to_owned(), async {
            yield_now().await;
            let conn =
                zbus::connection::Builder::address(upstream_socket.as_str())
                    .context_with("Invalid upstream addr", ea!(addr = upstream_socket))?
                    .build()
                    .await
                    .context_with("Error opening upstream connection", ea!(addr = upstream_socket))?;
            let (mut kill_tx, kill_rx) = oneshot::channel::<()>();
            spawn({
                let conn = conn.clone();
                let log = log.fork(ea!(graft = service, side = "up->down"));
                let downstream_conn = downstream_conn.clone();
                let upstream_conn = conn.clone();
                let client = client.to_owned();
                let reply_serial_lookup = reply_serial_lookup.clone();
                async move {
                    select!{
                        _ = handle_upstream(log, downstream_conn, upstream_conn, client, reply_serial_lookup) => {
                        }
                        _ = kill_tx.cancellation() => {
                        }
                    }
                }
            });
            return Ok((conn, Arc::new(kill_rx)));
        }).await {
            Ok((c, _)) => serverside_conn = c,
            Err(e) => {
                this_log.log_err(
                    loga::WARN,
                    loga::err(e.to_string()).context("Error opening upstream connection"),
                );
                downstream_conn
                    .send(
                        &zbus::fdo::Error::NoNetwork(format!("Error connecting to upstream bus"))
                            .create_reply(&header)
                            .unwrap(),
                    )
                    .await
                    .context("Error replying with error")?;
                continue 'next_msg;
            },
        };
        let mut out = zbus::message::Builder::from(header.clone());
        match server_lookup.try_get_with(service.to_owned(), async {
            yield_now().await;
            let resp =
                serverside_conn
                    .call_method(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        Some("org.freedesktop.DBus"),
                        "GetNameOwner",
                        &service,
                    )
                    .await
                    .context_with("Error looking up destination name", ea!(service = service))?;
            return Ok(resp.body().deserialize::<UniqueName<'_>>()?.to_owned()) as Result<_, loga::Error>;
        }).await {
            Ok(dest_service) => {
                out = out.destination(dest_service).unwrap();
            },
            Err(e) => {
                this_log.log_err(
                    loga::WARN,
                    loga::err(
                        e.to_string(),
                    ).context_with("Couldn't find upstream, replying with error", ea!(service = service)),
                );
                downstream_conn
                    .send(
                        &zbus::fdo::Error::NoServer(format!("Proxy couldn't find upstream service"))
                            .create_reply(&header)
                            .unwrap(),
                    )
                    .await
                    .context("Error replying with error in reverse direction")?;
                continue 'next_msg;
            },
        }

        // Replace serial with new dest-side serial and store for reply
        let new_serial = zbus::message::PrimaryHeader::new(zbus::message::Type::MethodCall, 100).serial_num();
        reply_serial_lookup.insert(new_serial, ReplyType::Remote(ReplyTypeRemote {
            original_serial: header.primary().serial_num(),
            is_access_camera: 'res: {
                let Some(((path, interface), member)) =
                    header.path().zip(header.interface()).zip(header.member()) else {
                        break 'res false;
                    };
                break 'res path == "/org/freedesktop/portal/desktop" && interface == "org.freedesktop.portal.Camera" &&
                    member == "AccessCamera";
            },
        })).await;
        out = out.serial(new_serial);

        // Replace sender with self
        if let Some(x) = serverside_conn.unique_name() {
            out = out.sender(x).unwrap();
        }

        // Build the message and send
        let msg = unsafe {
            out
                .build_raw_body(
                    msg.body().data(),
                    header.signature().clone(),
                    msg.data().fds().iter().map(|x| match x {
                        zbus::zvariant::Fd::Borrowed(_borrowed_fd) => panic!(),
                        zbus::zvariant::Fd::Owned(owned_fd) => zbus::zvariant::OwnedFd::from(
                            owned_fd.try_clone().unwrap(),
                        ),
                    }).collect(),
                )
                .unwrap()
        };
        this_log.log_with(
            loga::DEBUG,
            "Sending message",
            ea!(msg = msg.dbg_str(), body = String::from_utf8_lossy(&msg.data())),
        );
        serverside_conn.send(&msg).await.context("Error forwarding message")?;
    }
    return Ok(());
}

async fn main1() -> Result<(), loga::Error> {
    let args = vark::<Args>();
    let log = Log::new_root(if args.debug.is_some() {
        loga::DEBUG
    } else {
        loga::INFO
    });
    let mut downstream = vec![];
    for name in &args.register {
        let downstream_conn =
            zbus::connection::Builder::address(args.downstream.as_str())
                .context_with("Invalid downstream addr", ea!(addr = args.downstream))?
                .build()
                .await
                .context_with("Error opening downstream connection", ea!(addr = args.downstream))?;
        downstream_conn
            .request_name(name.as_str())
            .await
            .context(format!("Error registering name [{}] on downstream", name))?;
        downstream.push(handle_downstream(&log, args.upstream.clone(), name.clone(), downstream_conn.clone()));
    }
    let mut errors = vec![];
    for res in join_all(downstream).await {
        if let Err(e) = res {
            errors.push(e);
        }
    }
    if !errors.is_empty() {
        return Err(loga::agg_err("Task failures", errors));
    }
    return Ok(());
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match main1().await {
        Ok(_) => { },
        Err(e) => fatal(e),
    }
}
