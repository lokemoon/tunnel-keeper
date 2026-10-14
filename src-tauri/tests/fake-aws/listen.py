# The "plugin": binds the local port on both loopbacks and reports it opened.
#   ok     - holds the port until signalled
#   drop   - holds it briefly, then the session "ends" with an error
#   orphan - forks a detached child that keeps the port and ignores SIGTERM,
#            the way session-manager-plugin can outlive the CLI
import os, signal, socket, sys, time
port, mode = int(sys.argv[1]), sys.argv[2]

def bind():
    socks = []
    for fam, host in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
        s = socket.socket(fam, socket.SOCK_STREAM)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind((host, port)); s.listen(5); socks.append(s)
    return socks

if mode == "orphan":
    if os.fork() == 0:
        os.setsid()
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        keep = bind()
        time.sleep(120)
        sys.exit(0)
    time.sleep(0.4)
    print(f"Port {port} opened for sessionId fake.\nWaiting for connections...", flush=True)
    time.sleep(120)
    sys.exit(0)

keep = bind()
print(f"Port {port} opened for sessionId fake.\nWaiting for connections...", flush=True)
if mode == "drop":
    time.sleep(1.5)
    print("An error occurred (TargetNotConnected) when calling the StartSession operation: i-fake is not connected.", file=sys.stderr, flush=True)
    sys.exit(254)
time.sleep(120)
