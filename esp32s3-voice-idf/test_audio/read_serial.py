import serial, sys, time

port = sys.argv[1] if len(sys.argv) > 1 else 'COM4'
dur = float(sys.argv[2]) if len(sys.argv) > 2 else 25.0

s = serial.Serial(port, 115200, timeout=0.3)
s.dtr = False
s.rts = False
s.reset_input_buffer()
time.sleep(0.3)

end = time.time() + dur
buf = b''
while time.time() < end:
    try:
        data = s.read(4096)
    except Exception:
        continue
    if data:
        buf += data
        txt = data.decode('utf-8', 'replace')
        sys.stdout.write(txt)
        sys.stdout.flush()
s.close()
