#!/usr/bin/env python3
"""
Relay messages from XREMAP_SOCKET to /run/user/@UID/{basename(XREMAP_SOCKET)}.
"""

import argparse
import asyncio as aio
import logging
import os
import pwd
from functools import partial
from pathlib import Path

from dbus_next.aio import MessageBus
from dbus_next.constants import BusType

log = logging.getLogger('xremap')
TRACE = 5


async def main():
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.ArgumentDefaultsHelpFormatter
    )
    parser.add_argument(
        "-s", "--socket",
        metavar="XREMAP_SOCKET",
        default="/run/xremap/gnome.sock",
        help="Xremap socket path"
    )
    parser.add_argument(
        "-o", "--owner",
        default="xremap",
        help="Xremap socket owner"
    )
    parser.add_argument(
        "-v", "--verbose",
        action="count",
        default=0,
        help="Enable debug logging. Repeat for more detail."
    )
    args = parser.parse_args()
    xremap_socket = Path(args.socket)
    add_trace_logging_level()
    debug = TRACE if args.verbose > 1 else logging.DEBUG
    logging.basicConfig(level=debug if args.verbose else logging.INFO)

    bus = await MessageBus(bus_type=BusType.SYSTEM).connect()
    try:
        await monitor_sessions(bus, xremap_socket, args.owner)
    finally:
        if xremap_socket.exists():
            xremap_socket.unlink()
            log.debug("Removed %s", xremap_socket)


async def monitor_sessions(bus: MessageBus, xremap_socket: Path, owner: str):
    """Watch for SessionNew signals on D-Bus"""
    log.debug("Monitoring D-Bus for new user sessions...")
    if not xremap_socket.parent.is_dir():
        raise ValueError(f"Socket directory not found: {xremap_socket}")

    proxy = bus.get_proxy_object(
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        await bus.introspect("org.freedesktop.login1", "/org/freedesktop/login1"),
    )
    manager = proxy.get_interface("org.freedesktop.login1.Manager")
    active = {}
    the_end = aio.get_event_loop().create_future()

    def on_session_new(session_id, session_path):
        assert session_id not in active, session_id
        task = active[session_id] = aio.create_task(
            handle_new_session(bus, session_id, session_path, xremap_socket, owner))
        task.add_done_callback(partial(on_session_end, session_id))

    def on_session_removed(session_id, session_path):
        if session_id in active:
            log.trace("Session %s removed -> cancel monitor", session_id)
            active[session_id].cancel()

    def on_session_end(session_id, task: aio.Task):
        active.pop(session_id)
        try:
            task.result()
            log.debug("Session %s monitor completed", session_id)
        except aio.CancelledError:
            log.debug("Session %s monitor cancelled", session_id)
        except Exception:
            log.exception("Session %s monitor failed", session_id)
            the_end.set_result(True)

    sessions = await manager.call_list_sessions()
    for session_id, uid, user, seat_id, session_path in sessions:
        if seat_id:
            log.debug(f"Existing session: {session_id} (uid={uid}, seat={seat_id})")
            assert session_id not in active, session_id
            task = active[session_id] = aio.create_task(
                handle_session(session_id, uid, xremap_socket, owner))
            task.add_done_callback(partial(on_session_end, session_id))

    manager.on_session_new(on_session_new)
    manager.on_session_removed(on_session_removed)
    await the_end


async def handle_new_session(bus: MessageBus, session_id, session_path: str, xremap_socket: Path, owner: str):
    session_proxy = bus.get_proxy_object(
        "org.freedesktop.login1",
        session_path,
        await bus.introspect("org.freedesktop.login1", session_path),
    )
    session = session_proxy.get_interface("org.freedesktop.login1.Session")
    seat_id = (await session.get_seat())[0]
    if seat_id:
        uid = (await session.get_user())[0]
        await handle_session(session_id, uid, xremap_socket, owner)
    else:
        log.debug("Ignoring unseated session %s", session_id)


async def handle_session(session_id, uid: int, xremap_socket: Path, owner: str):
    log.info("Monitoring session %s for user %s", session_id, uid)
    user_socket = Path(f"/run/user/{uid}/{xremap_socket.name}")

    async def handle_connect(*args):
        log.trace("Session %s relaying to %s", session_id, user_socket)
        await relay_message(user_socket, *args)
        log.trace("Session %s relay completed", session_id)

    server = await aio.start_unix_server(handle_connect, xremap_socket)
    user = pwd.getpwnam(owner)
    os.chown(xremap_socket, user.pw_uid, user.pw_gid)
    os.chmod(xremap_socket, 0o660)
    async with server:
        log.debug("Session %s serving %s -> %s", session_id, xremap_socket, user_socket)
        await server.serve_forever()


async def relay_message(socket_path: Path, in_reader: aio.StreamReader, in_writer: aio.StreamWriter):
    try:
        if not socket_path.exists():
            log.trace("Abort relay: %s does not exist", socket_path)
            return
        out_reader, out_writer = await aio.open_unix_connection(socket_path)
        try:
            async for request in in_reader:
                log.trace("Message: %r", request)
                out_writer.write(request)
                await out_writer.drain()
                async for response in out_reader:
                    log.trace("Response: %r", response)
                    in_writer.write(response)
                    await in_writer.drain()
        finally:
            out_writer.close()
            await out_writer.wait_closed()
    finally:
        in_writer.close()
        await in_writer.wait_closed()


class add_trace_logging_level():
    def trace(self, message, *args, **kwargs):
        if self.isEnabledFor(TRACE):
            self._log(TRACE, message, args, **kwargs)

    logging.Logger.trace = trace
    logging.addLevelName(TRACE, "TRACE")


class CannotConnect(Exception):
    pass


if __name__ == "__main__":
    try:
        aio.run(main())
    except KeyboardInterrupt:
        log.debug("The end.")
