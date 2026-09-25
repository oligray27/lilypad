"""LilyPad for Decky: runs LilyPad's headless tracking engine in Steam Gaming Mode.

The engine (bin/lilypad-engine, built from this repository's lilypad-engine crate) does all the
work: detecting games, recording sessions durably, recovering after a crash, submitting to
FrogLog. It shares its data directory with the LilyPad desktop app, and only one of the two
tracks at a time: when the desktop app starts, the engine hands over and exits, and this
supervisor restarts it to wait until Gaming Mode is back.

This file only supervises the engine and relays its line-delimited JSON protocol: `call`
forwards a request from the panel, and every engine event is re-emitted to the frontend as
`lilypad_event`.
"""

import asyncio
import json
import os

import decky

ENGINE = os.path.join(decky.DECKY_PLUGIN_DIR, "bin", "lilypad-engine")
EXIT_YIELDED = 3  # the engine handed tracking over to the desktop app
REQUEST_TIMEOUT = 90  # seconds; creating a game and uploading its sessions can take a while


class Plugin:
    async def _main(self):
        self.proc = None
        self.next_id = 1
        self.waiting = {}
        self.stopping = False
        self.supervisor = asyncio.get_event_loop().create_task(self._supervise())
        decky.logger.info("LilyPad plugin loaded")

    async def _unload(self):
        self.stopping = True
        self.supervisor.cancel()
        await self._stop_engine()
        decky.logger.info("LilyPad plugin unloaded")

    async def _uninstall(self):
        await self._unload()

    async def call(self, cmd: str, args: dict = None):
        """Sends one request to the engine and returns its reply: {"ok": bool, "result"|"error"}."""
        proc = self.proc
        if proc is None or proc.returncode is not None or proc.stdin is None:
            return {"ok": False, "error": "LilyPad is starting. Try again in a moment."}
        request_id = self.next_id
        self.next_id += 1
        future = asyncio.get_event_loop().create_future()
        self.waiting[request_id] = future
        line = json.dumps({"id": request_id, "cmd": cmd, "args": args or {}}) + "\n"
        try:
            proc.stdin.write(line.encode())
            await proc.stdin.drain()
            return await asyncio.wait_for(future, REQUEST_TIMEOUT)
        except asyncio.TimeoutError:
            return {"ok": False, "error": "LilyPad did not answer in time."}
        except (BrokenPipeError, ConnectionResetError):
            return {"ok": False, "error": "LilyPad restarted. Try again in a moment."}
        finally:
            self.waiting.pop(request_id, None)

    async def _supervise(self):
        while not self.stopping:
            code = await self._run_engine()
            if self.stopping:
                return
            # After a hand-over the engine only waits for the lock, so restart it promptly;
            # after anything else, back off a little so a crash loop can't spin.
            delay = 2 if code == EXIT_YIELDED else 10
            decky.logger.info(f"engine exited with {code}; restarting in {delay}s")
            await decky.emit("lilypad_event", {"event": "engine_stopped", "code": code})
            await asyncio.sleep(delay)

    async def _run_engine(self):
        # Zip extraction does not keep file modes; the loader normally fixes that, but make sure.
        if os.path.exists(ENGINE) and not os.access(ENGINE, os.X_OK):
            try:
                os.chmod(ENGINE, 0o755)
            except OSError as e:
                decky.logger.error(f"{ENGINE} is not executable and could not be fixed: {e}")
        env = dict(os.environ)
        env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
        env.setdefault("RUST_LOG", "lilypad_core=info,lilypad_engine=info")
        log_path = os.path.join(decky.DECKY_PLUGIN_LOG_DIR, "engine.log")
        with open(log_path, "ab") as log:
            try:
                self.proc = await asyncio.create_subprocess_exec(
                    ENGINE,
                    stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=log,
                    env=env,
                    # Protocol lines can be long (a whole library listing), beyond the 64 KiB default.
                    limit=16 * 1024 * 1024,
                )
            except OSError as e:
                decky.logger.error(f"could not start {ENGINE}: {e}")
                return -1
            decky.logger.info(f"engine started (pid {self.proc.pid})")
            try:
                await self._relay(self.proc)
            finally:
                code = await self.proc.wait()
                for future in self.waiting.values():
                    if not future.done():
                        future.set_result({"ok": False, "error": "LilyPad restarted. Try again in a moment."})
                self.waiting.clear()
                self.proc = None
            return code

    async def _relay(self, proc):
        while True:
            line = await proc.stdout.readline()
            if not line:
                return
            try:
                message = json.loads(line)
            except ValueError:
                decky.logger.warning(f"engine wrote a non-protocol line: {line[:200]!r}")
                continue
            if "event" in message:
                await decky.emit("lilypad_event", message)
            elif "id" in message:
                future = self.waiting.get(message["id"])
                if future is not None and not future.done():
                    future.set_result(message)

    async def _stop_engine(self):
        proc = self.proc
        if proc is None or proc.returncode is not None:
            return
        # Closing stdin makes the engine exit cleanly; anything in progress is durable.
        if proc.stdin is not None:
            proc.stdin.close()
        try:
            await asyncio.wait_for(proc.wait(), 5)
        except asyncio.TimeoutError:
            proc.terminate()
