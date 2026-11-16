import os
import re
import subprocess

import gdb

supported_offsets = {
    "4.1.0.16": 0x6586C90,
    "4.1.13.26": 0x87B3CF0,
}
actual_version = subprocess.check_output(
    ["dpkg-query", "-W", "-f=${Version}", "wechat"], text=True
).strip()
if actual_version not in supported_offsets:
    raise RuntimeError("Unsupported WeChat version: " + actual_version)

gdb.execute("set pagination off")
gdb.execute("set confirm off")

wechat_pid = gdb.selected_inferior().pid
wechat_base = None
with open("/proc/" + str(wechat_pid) + "/maps", encoding="utf-8") as mappings:
    for mapping in mappings:
        fields = mapping.split()
        if len(fields) >= 6 and fields[-1] == "/opt/wechat/wechat":
            if int(fields[2], 16) == 0:
                wechat_base = int(fields[0].split("-")[0], 16)
                break
if wechat_base is None:
    raise RuntimeError("The WeChat executable mapping was not found")

function_address = wechat_base + supported_offsets[actual_version]
function_signature = bytes.fromhex(
    "554157415641554154534883ec2889cd4189d74989f44889fb4889f7"
)
actual_signature = (
    gdb.selected_inferior()
    .read_memory(function_address, len(function_signature))
    .tobytes()
)
if actual_signature != function_signature:
    raise RuntimeError("The WeChat database key function signature did not match")


class CipherKeyBreakpoint(gdb.Breakpoint):
    def stop(self):
        try:
            data_address = int(gdb.parse_and_eval("$rsi"))
            inferior = gdb.selected_inferior()
            key_address = int.from_bytes(
                inferior.read_memory(data_address + 8, 8).tobytes(), "little"
            )
            key_length = int.from_bytes(
                inferior.read_memory(data_address + 16, 8).tobytes(), "little"
            )
            if key_length != 32 or key_address < 4096:
                return False
            key_hex = inferior.read_memory(key_address, 32).tobytes().hex()
            if re.fullmatch("[0-9a-f]{64}", key_hex) is None:
                return False
            descriptor = os.open(
                "/var/lib/mimicwx/wechat-key",
                os.O_WRONLY | os.O_CREAT | os.O_TRUNC,
                0o600,
            )
            with os.fdopen(descriptor, "w", encoding="ascii") as key_file:
                os.fchown(key_file.fileno(), 1000, 1000)
                os.fchmod(key_file.fileno(), 0o600)
                key_file.write(key_hex)
            print("WeChat database key captured and saved.")
            gdb.post_event(self.finish)
        except (gdb.error, OSError, ValueError) as error:
            print("Key capture failed: " + str(error))
        return False

    def finish(self):
        gdb.execute("delete breakpoints")
        gdb.execute("detach")
        gdb.execute("quit")


CipherKeyBreakpoint("*" + hex(function_address))
print("Validated database key extraction for WeChat " + actual_version + ".")
print("Waiting for WeChat login to initialize its database.")
gdb.execute("continue")
