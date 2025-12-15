#!/usr/bin/env python3
"""
Relay messages from XREMAP_SOCKET to /run/user/@UID/{basename(XREMAP_SOCKET)}.
"""

import argparse
import asyncio as aio
import logging
import os
import pwd
from dataclasses import dataclass
from pathlib import Path

from dbus_next.aio import MessageBus, ProxyObject
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
        "-u", "--user-socket",
        default="/run/user/{uid}/gnome.sock",
        help="User socket pattern"
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
    await monitor_sessions(bus, xremap_socket, args.owner, args.user_socket)


async def monitor_sessions(bus: MessageBus, xremap_socket: Path, owner: str, user_socket_pattern: str):
    """Watch for SessionNew signals on D-Bus"""
    log.debug("Monitoring D-Bus for new user sessions...")
    proxy = bus.get_proxy_object(
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        await bus.introspect("org.freedesktop.login1", "/org/freedesktop/login1"),
    )
    manager = proxy.get_interface("org.freedesktop.login1.Manager")
    sessions = {}
    active_sessions = {}

    async def on_session_new(session_id, session_path):
        assert session_id not in sessions, session_id
        session = await handle_new_session(
            bus, session_id, session_path, user_socket_pattern, on_properties_changed)
        if session is not None:
            sessions[session_id] = session
            if await session.is_active():
                active_sessions[session_id] = session

    def on_properties_changed(session, is_active):
        if is_active:
            log.info("Activated session %s", session.id)
            active_sessions[session.id] = session
        else:
            log.info("Deactivated session %s", session.id)
            active_sessions.pop(session.id, None)

    def on_session_removed(session_id, session_path):
        session = sessions.pop(session_id, None)
        active_ = active_sessions.pop(session_id, None)
        active = " active" if active_ is not None else ""
        if session is not None:
            session.finalize()
            log.trace("Session%s %s removed", active, session_id)
        elif active:
            active_.finalize()
            log.warning("Discarded unknown active session: %s", session_id)

    def get_active_session():
        if len(active_sessions) > 1:
            log.warning("Unexpected: multiple active sessions: %s", active_sessions.keys())
            return None
        return next(iter(active_sessions.values()), None)

    current_sessions = await manager.call_list_sessions()
    for session_id, uid, user, seat_id, session_path in current_sessions:
        if seat_id:
            log.debug(f"Existing session: {session_id} (uid={uid}, seat={seat_id})")
            assert session_id not in sessions, session_id
            session = sessions[session_id] = await handle_session(
                bus, session_id, session_path, uid, user_socket_pattern, on_properties_changed)
            if await session.is_active():
                active_sessions[session_id] = session

    manager.on_session_new(on_session_new)
    manager.on_session_removed(on_session_removed)
    await socket_server(xremap_socket, owner, get_active_session)


async def handle_new_session(
    bus: MessageBus,
    session_id: str,
    session_path: str,
    *args,
):
    proxy = bus.get_proxy_object(
        "org.freedesktop.login1",
        session_path,
        await bus.introspect("org.freedesktop.login1", session_path),
    )
    bus_session = proxy.get_interface("org.freedesktop.login1.Session")
    seat_id = (await bus_session.get_seat())[0]
    if seat_id:
        uid = (await bus_session.get_user())[0]
        return await handle_session(bus, session_id, session_path, uid, *args)
    log.debug("Ignoring unseated session %s", session_id)
    return None


async def handle_session(
    bus: MessageBus,
    session_id: str,
    session_path: str,
    uid: int,
    user_socket_pattern: str,
    on_properties_changed: callable,
):
    proxy = bus.get_proxy_object(
        "org.freedesktop.login1",
        session_path,
        await bus.introspect("org.freedesktop.login1", session_path),
    )
    user_socket = Path(user_socket_pattern.format(uid=uid))
    session = Session(session_id, user_socket, proxy, on_properties_changed)
    active = " active" if await session.is_active() else ""
    log.info("Monitoring%s session %s for user %s", active, session_id, uid)
    return session


async def socket_server(xremap_socket: Path, owner: str, get_active_session):
    async def handle_connect(*args):
        session = get_active_session()
        if session is None:
            log.trace("No active session")
            return
        log.trace("Relaying to session %s on %s", session.id, session.user_socket)
        await relay_message(session.user_socket, *args)

    if not xremap_socket.parent.is_dir():
        raise ValueError(f"Socket directory not found: {xremap_socket}")
    server = await aio.start_unix_server(handle_connect, xremap_socket)
    user = pwd.getpwnam(owner)
    os.chown(xremap_socket, user.pw_uid, user.pw_gid)
    os.chmod(xremap_socket, 0o660)
    try:
        async with server:
            log.debug("Serving to %s", xremap_socket)
            await server.serve_forever()
    finally:
        if xremap_socket.exists():
            xremap_socket.unlink()
            log.debug("Removed %s", xremap_socket)


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
    log.trace("Relay to %s completed", socket_path)


class add_trace_logging_level():
    def trace(self, message, *args, **kwargs):
        if self.isEnabledFor(TRACE):
            self._log(TRACE, message, args, **kwargs)

    logging.Logger.trace = trace
    logging.addLevelName(TRACE, "TRACE")


@dataclass
class Session:
    id: str
    user_socket: Path
    proxy: ProxyObject
    on_properties_changed: callable

    def __post_init__(self):
        self.bus_session = self.proxy.get_interface("org.freedesktop.login1.Session")
        self.props = self.proxy.get_interface("org.freedesktop.DBus.Properties")
        self.props.on_properties_changed(self._on_properties_changed)

    async def is_active(self):
        return await self.bus_session.get_active()

    def _on_properties_changed(self, interface, changed, invalidated):
        log.trace("Session %s properties changed: %r", self.id, changed)
        active = changed.get("Active")
        if active is not None:
            self.on_properties_changed(self, active.value)

    def finalize(self):
        try:
            self.props.off_properties_changed(self._on_properties_changed)
        except Exception:
            log.exception("Unable to finalize session %s", self.id)


class CannotConnect(Exception):
    pass


if __name__ == "__main__":
    try:
        aio.run(main())
    except KeyboardInterrupt:
        log.debug("The end.")
