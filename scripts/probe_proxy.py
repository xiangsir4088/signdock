"""探测：目标应用是否遵循 Windows 系统代理。

原理：起一个纯 TCP 的 CONNECT 隧道代理（不解 TLS、不读请求内容），临时把它设为
系统代理，然后把窗口期内的 CONNECT 目标主机打印出来。能看到 CONNECT == 应用走系统代理；
一条都没有 == 应用忽略系统代理（自定义代理/PAC/直连）。

只读安全：不改产品文件、不解密任何流量；退出时（含异常）必定还原代理设置。

用法：python scripts/probe_proxy.py [秒数] [端口]
"""
import socket
import sys
import threading
import time
import winreg
import ctypes
from concurrent.futures import ThreadPoolExecutor

INTERNET_SETTINGS = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings"
INTERNET_OPTION_SETTINGS_CHANGED = 39
INTERNET_OPTION_REFRESH = 37


def read_proxy():
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, INTERNET_SETTINGS) as key:
        enable = _get(key, "ProxyEnable", 0)
        server = _get(key, "ProxyServer", None)
        return enable, server


def _get(key, name, default):
    try:
        return winreg.QueryValueEx(key, name)[0]
    except FileNotFoundError:
        return default


def write_proxy(enable, server):
    with winreg.OpenKey(winreg.HKEY_CURRENT_USER, INTERNET_SETTINGS, 0, winreg.KEY_SET_VALUE) as key:
        winreg.SetValueEx(key, "ProxyEnable", 0, winreg.REG_DWORD, enable)
        if server is None:
            try:
                winreg.DeleteValue(key, "ProxyServer")
            except FileNotFoundError:
                pass
        else:
            winreg.SetValueEx(key, "ProxyServer", 0, winreg.REG_SZ, server)
    wininet = ctypes.WinDLL("wininet.dll")
    for option in (INTERNET_OPTION_SETTINGS_CHANGED, INTERNET_OPTION_REFRESH):
        wininet.InternetSetOptionW(0, option, 0, 0)


def splice(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def handle(client, hosts):
    host = None
    with client:
        try:
            head = b""
            while b"\r\n\r\n" not in head:
                chunk = client.recv(4096)
                if not chunk:
                    return
                head += chunk
            first = head.split(b"\r\n", 1)[0].decode("latin-1")
            parts = first.split()
            if len(parts) == 3 and parts[0].upper() == "CONNECT":
                host = parts[1]
                hosts.append(host)
                print(f"CONNECT {host}", flush=True)
                client.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
                target = socket.create_connection((parts[1].split(":")[0], int(parts[1].rsplit(":", 1)[-1])), 8)
            else:
                hosts.append(f"PLAINTEXT {first[:60]}")
                print(f"PLAINTEXT {first}", flush=True)
                return
        except Exception as exc:  # noqa: BLE001 - 探测脚本，记录后继续
            print(f"  ({type(exc).__name__} for {host})", flush=True)
            return
        with target:
            with ThreadPoolExecutor(2) as pool:
                pool.submit(splice, client, target)
                pool.submit(splice, target, client)


def main():
    seconds = int(sys.argv[1]) if len(sys.argv) > 1 else 60
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8899
    original = read_proxy()
    print(f"当前代理设置: ProxyEnable={original[0]} ProxyServer={original[1]}")
    hosts = []
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", port))
    listener.listen(64)
    stop = threading.Event()

    def serve():
        listener.settimeout(1.0)
        while not stop.is_set():
            try:
                client, _ = listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=handle, args=(client, hosts), daemon=True).start()

    threading.Thread(target=serve, daemon=True).start()
    try:
        write_proxy(1, f"127.0.0.1:{port}")
        print(f"代理已临时指向 127.0.0.1:{port}，窗口 {seconds}s —— 请在目标应用里点一下刷新/发一条消息")
        for i in range(seconds):
            time.sleep(1)
            if (i + 1) % 10 == 0:
                print(f"  ... {i + 1}s，已捕获 {len(hosts)} 条", flush=True)
    finally:
        stop.set()
        listener.close()
        write_proxy(*original)
        print(f"代理已还原: ProxyEnable={original[0]} ProxyServer={original[1]}")
    uniq = {}
    for h in hosts:
        uniq[h] = uniq.get(h, 0) + 1
    print(f"\n合计 {len(hosts)} 条连接，去重后 {len(uniq)} 个目标：")
    for h, n in sorted(uniq.items(), key=lambda kv: -kv[1]):
        print(f"  {n:>4}  {h}")


if __name__ == "__main__":
    main()
