"""一次性可行性验证（不进产品代码）：WorkBuddy/CodeBuddy 是否做证书固定。

做法：临时 CA 导入当前用户信任根 + 系统代理指向本地端口；只对白名单域名解 TLS 并打印
「有哪些请求头」，其余域名继续纯隧道。输出绝不包含 header 取值（authorization 只打印
长度与 sha256 前 8 位指纹）。退出时（含异常）必定还原代理并删除临时 CA。

窗口期内白名单请求会以 502 失败——这是探测的代价，60~120s 后恢复正常。

用法：python scripts/spike_mitm.py [秒数] [端口]
"""
import datetime
import hashlib
import os
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from probe_proxy import read_proxy, write_proxy  # noqa: E402

MITM_HOSTS = ("codebuddy.cn", "workbuddy.cn")


def make_ca():
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "SignDock Capture CA (temporary)")])
    now = datetime.datetime.now(datetime.UTC)
    cert = (
        x509.CertificateBuilder()
        .subject_name(name).issuer_name(name).public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(days=1))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(True, 0), critical=True)
        .add_extension(x509.KeyUsage(True, False, False, False, False, True, True, False, False), critical=True)
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("signdock.invalid")]), critical=False)
        .add_extension(x509.SubjectKeyIdentifier.from_public_key(key.public_key()), critical=False)
        .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(key.public_key()), critical=False)
        .sign(key, hashes.SHA256())
    )
    return key, cert


def leaf_for(ca_key, ca_cert, host, workdir):
    key = ec.generate_private_key(ec.SECP256R1())
    now = datetime.datetime.now(datetime.UTC)
    cert = (
        x509.CertificateBuilder()
        .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, host)]))
        .issuer_name(ca_cert.subject).public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - datetime.timedelta(days=1))
        .not_valid_after(now + datetime.timedelta(days=1))
        .add_extension(x509.BasicConstraints(False, None), critical=True)
        .add_extension(x509.KeyUsage(True, False, False, False, False, False, False, False, False), critical=True)
        .add_extension(x509.ExtendedKeyUsage([x509.OID_SERVER_AUTH]), critical=False)
        .add_extension(x509.SubjectAlternativeName([x509.DNSName(host)]), critical=False)
        .add_extension(x509.AuthorityKeyIdentifier.from_issuer_public_key(ca_key.public_key()), critical=False)
        .sign(ca_key, hashes.SHA256())
    )
    stem = hashlib.sha1(host.encode()).hexdigest()[:12]
    crt = os.path.join(workdir, f"{stem}.crt")
    pemkey = os.path.join(workdir, f"{stem}.key")
    with open(crt, "wb") as fh:
        fh.write(cert.public_bytes(serialization.Encoding.PEM))
        fh.write(ca_cert.public_bytes(serialization.Encoding.PEM))
    with open(pemkey, "wb") as fh:
        fh.write(key.private_bytes(serialization.Encoding.PEM,
                                   serialization.PrivateFormat.TraditionalOpenSSL,
                                   serialization.NoEncryption()))
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(crt, pemkey)
    return ctx


def describe(value):
    return f"len={len(value)} sha256={hashlib.sha256(value.encode()).hexdigest()[:8]}"


def handle(client, ca_key, ca_cert, workdir, log):
    peer = None
    with client:
        try:
            client.settimeout(20)
            head = b""
            while b"\r\n\r\n" not in head:
                chunk = client.recv(4096)
                if not chunk:
                    return
                head += chunk
            first = head.split(b"\r\n", 1)[0].decode("latin-1")
            parts = first.split()
            if len(parts) != 3 or parts[0].upper() != "CONNECT":
                log.append(f"PLAINTEXT {first[:60]}")
                return
            authority = parts[1]
            hostname = authority.split(":")[0]
            port = int(authority.rsplit(":", 1)[-1])
            client.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
            log.append(f"CONNECT {authority}")
            print(f"CONNECT {authority}{'  [将解 TLS]' if any(hostname == h or hostname.endswith('.' + h) for h in MITM_HOSTS) else ''}", flush=True)
            if not any(hostname == h or hostname.endswith("." + h) for h in MITM_HOSTS):
                peer = socket.create_connection((hostname, port), 8)
                splice(client, peer)
                return
            print(f"MITM  {authority}", flush=True)
            ctx = leaf_for(ca_key, ca_cert, hostname, workdir)
            try:
                tls = ctx.wrap_socket(client, server_side=True)
            except ssl.SSLError as exc:
                log.append(f"TLS-FAIL {hostname}: {exc}")
                print(f"TLS-FAIL {hostname}: {exc}  <-- 证书固定/校验拒绝", flush=True)
                return
            with tls:
                req = b""
                tls.settimeout(10)
                while b"\r\n\r\n" not in req:
                    chunk = tls.recv(4096)
                    if not chunk:
                        break
                    req += chunk
                text = req.decode("latin-1")
                lines = [l for l in text.split("\r\n") if l]
                log.append(f"REQUEST {hostname} {lines[0][:80]}")
                print(f"  OK 握手成功 {hostname} :: {lines[0][:80]}", flush=True)
                for line in lines[1:]:
                    if ":" not in line:
                        continue
                    name, _, value = line.partition(":")
                    name = name.strip().lower()
                    if name == "authorization":
                        print(f"  authorization: {describe(value.strip())}", flush=True)
                        log.append(f"  authorization {describe(value.strip())}")
                    elif name == "cookie":
                        print(f"  cookie: {describe(value.strip())}", flush=True)
                        log.append(f"  cookie {describe(value.strip())}")
                    else:
                        log.append(f"  header {name}")
                tls.sendall(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        except Exception as exc:  # noqa: BLE001
            print(f"  ({type(exc).__name__})", flush=True)
        finally:
            if peer:
                peer.close()


def splice(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass


def run_ps(cmd):
    r = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command", cmd],
                       capture_output=True, text=True, errors="replace", timeout=90)
    return r.returncode, (r.stdout or "").strip(), (r.stderr or "").strip()


CA_SUBJECT = "CN=SignDock Capture CA (temporary)"


def find_thumbprint():
    rc, out, _ = run_ps(f"(Get-ChildItem Cert:\\CurrentUser\\Root | Where-Object {{ $_.Subject -eq '{CA_SUBJECT}' }} | Select-Object -First 1 -ExpandProperty Thumbprint)")
    return out if rc == 0 and out else ""


def import_ca(cert_der_path):
    # Import-Certificate 在 -NonInteractive 下被拒绝（"不允许使用 UI"）；certutil 加当前用户 Root 存储不弹窗，实测无提示
    rc, out, err = run_ps(f"certutil -addstore -user Root '{cert_der_path}' | Out-String")
    print(f"certutil -addstore rc={rc} err={err[:200]}")
    thumb = find_thumbprint()
    print(f"CA 已导入，thumbprint={thumb or '(失败)'}")
    return thumb


def remove_ca(thumb):
    # 只用 Remove-Item：certutil -delstore 会弹交互式确认并卡死（实测 90s 超时）
    if thumb:
        run_ps(f"Remove-Item -Path 'Cert:\\CurrentUser\\Root\\{thumb}' -ErrorAction SilentlyContinue")
    left = find_thumbprint()
    print(f"临时 CA 清理：{'已删除' if not left else f'残留 {left}，请手动删除（certlm.msc → 当前用户 → 受信任的根证书颁发机构）'}")


def selftest(port):
    """用一个信任 Windows 根存储的客户端走代理，验证证书链是否被接受（等价于 Chromium 的校验路径）。"""
    import http.client

    ctx = ssl.create_default_context()  # Windows 上会加载系统 ROOT 存储
    try:
        conn = http.client.HTTPSConnection("127.0.0.1", port, context=ctx, timeout=12)
        conn.set_tunnel("www.codebuddy.cn", 443)
        conn.request("GET", "/")
        status = conn.getresponse().status
        print(f"SELFTEST 握手成功，代理返回 status={status}（502 属预期）→ Windows 校验器接受我们的链")
    except ssl.SSLCertVerificationError as exc:
        print(f"SELFTEST 证书被拒: {exc.verify_message if hasattr(exc, 'verify_message') else exc}")
    except Exception as exc:  # noqa: BLE001
        print(f"SELFTEST 失败: {type(exc).__name__}: {exc}")


def main():
    seconds = int(sys.argv[1]) if len(sys.argv) > 1 else 120
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8899
    original = read_proxy()
    ca_key, ca_cert = make_ca()
    log = []
    thumb = ""
    listener = None
    stop = threading.Event()
    try:
        with tempfile.TemporaryDirectory(prefix="signdock-spike-") as workdir:
            der = os.path.join(workdir, "ca.der")
            with open(der, "wb") as fh:
                fh.write(ca_cert.public_bytes(serialization.Encoding.DER))
            thumb = import_ca(der)
            if not thumb:
                print("无法导入临时 CA，探测中止（代理未改动）")
                return
            listener = socket.socket()
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", port))
            listener.listen(64)

            def serve():
                listener.settimeout(1.0)
                while not stop.is_set():
                    try:
                        client, _ = listener.accept()
                    except socket.timeout:
                        continue
                    except OSError:
                        break
                    threading.Thread(target=handle, args=(client, ca_key, ca_cert, workdir, log), daemon=True).start()

            threading.Thread(target=serve, daemon=True).start()
            write_proxy(1, f"127.0.0.1:{port}")
            selftest(port)
            print(f"代理 → 127.0.0.1:{port}，窗口 {seconds}s。请在 WorkBuddy 里发消息或点刷新（该请求会失败）")
            for i in range(seconds):
                time.sleep(1)
                if (i + 1) % 15 == 0:
                    print(f"  ... {i + 1}s，已捕获 {len(log)} 条", flush=True)
    finally:
        stop.set()
        if listener:
            listener.close()
        write_proxy(*original)
        print(f"代理已还原: ProxyEnable={original[0]} ProxyServer={original[1]}")
        remove_ca(thumb)
        after = read_proxy()
        print(f"复核: ProxyEnable={after[0]} ProxyServer={after[1]} CA 残留={'有' if find_thumbprint() else '无'}")
    print(f"\n=== 结论（{len(log)} 条）===")
    for line in log:
        print(" ", line)


if __name__ == "__main__":
    main()
