import subprocess, sys
r = subprocess.run(["cargo", "check", "--workspace", "--all-targets"],
                   cwd=r"C:\Users\Mr-Music\Documents\GitHub\RDPiO",
                   capture_output=True, text=True)
print("GATE_RC=", r.returncode)
if r.returncode != 0:
    err = r.stderr or r.stdout
    for line in err.splitlines():
        if "error" in line.lower():
            print(line[:120])
