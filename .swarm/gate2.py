import subprocess, re
r = subprocess.run(["cargo", "check", "-p", "rdp-client"],
                   cwd=r"C:\Users\Mr-Music\Documents\GitHub\RDPiO",
                   capture_output=True, text=True)
err = r.stderr or ""
codes = re.findall(r"error\[(E\d+)\]", err)
from collections import Counter
c = Counter(codes)
print("RC", r.returncode)
print("N", len(codes))
for code, cnt in c.most_common(12):
    print(code, cnt)
lines = [l.strip() for l in err.splitlines() if "error[" in l]
for l in lines[:6]:
    print("L:", l[:110])
