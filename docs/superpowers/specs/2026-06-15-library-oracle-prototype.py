#!/usr/bin/env python3
"""Full-loop automated library oracle: generate BOTH drivers (source + Rust),
run both, diff. Nothing hand-written."""
import json, os, re, subprocess, sys, urllib.request, pathlib

KEY = os.environ["RUSTYFI_LLM_API_KEY"]
ROOT = pathlib.Path("/Users/clubpenguin/Documents/TROMBONE/rustyfi")
OUT = ROOT / "bench/.work/out/itsdangerous"
SRC = ROOT / "bench/.work/src/itsdangerous/src"
MAIN = OUT / "src/main.rs"
MODS = "pub mod test_itsdangerous;\npub mod docs;\npub mod itsdangerous;\n"
RUST_API = pathlib.Path("/tmp/rust_api.txt").read_text()

PY_API = """Python library `itsdangerous`, import: `from itsdangerous.signer import Signer`
- Signer(secret_key: str)            # construct
- signer.sign(value: str) -> bytes   # returns value + b"." + signature
- signer.unsign(signed: bytes) -> bytes   # raises itsdangerous.BadSignature on a forged/tampered value
"""

def deepseek(system, user):
    body = json.dumps({"model": "deepseek-chat", "temperature": 0.0,
                       "messages": [{"role": "system", "content": system},
                                    {"role": "user", "content": user}]}).encode()
    req = urllib.request.Request("https://api.deepseek.com/chat/completions", data=body,
                                 headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=180) as r:
        return json.load(r)["choices"][0]["message"]["content"]

def strip_fences(s):
    s = s.strip(); s = re.sub(r"^```[a-zA-Z]*\n", "", s); s = re.sub(r"\n```$", "", s)
    return s.strip()

# --- 1. generate the SOURCE (python) driver from the API ---
src_driver = strip_fences(deepseek(
    "You are an expert Python programmer. Output only Python code.",
    f"""Write a Python driver that exercises this library's public API with a few
DETERMINISTIC checks and prints one labeled line per check.

{PY_API}

Requirements:
- Cover: sign (print `sign=<hex>` using .hex()), unsign round-trip (print `roundtrip=<decoded str>`),
  and tamper-rejection (flip the last byte of a signed value, unsign it, print `tamper=rejected` if it
  raises else `tamper=ACCEPTED`).
- Use a fixed secret key "my-secret-key" and value "hello-world". No time, no randomness.
- Output ONLY Python code, no fences."""))
pathlib.Path("/tmp/gen_src.py").write_text(src_driver)
golden = subprocess.run(["python3", "/tmp/gen_src.py"], env={**os.environ, "PYTHONPATH": str(SRC)},
                        capture_output=True, text=True)
golden_out = golden.stdout.strip()

# --- 2. port the source driver to a RUST driver against the contract ---
def gen_rust(extra=""):
    return strip_fences(deepseek(
        "You are an expert Rust programmer. Output only Rust code.",
        f"""Port this Python driver to Rust against the crate API below. Output ONLY a Rust
`fn main() {{...}}` (no mod decls, no fences). Print the EXACT same labels/order; bytes as
lowercase hex; reach the type as `crate::itsdangerous::Signer`.

Rust API:
{RUST_API}

Python driver to port:
{src_driver}
{extra}"""))

backup = MAIN.read_text()
rust_out, generated, ok = "<none>", "", False
try:
    extra = ""
    for attempt in range(2):
        generated = gen_rust(extra)
        MAIN.write_text(MODS + generated + "\n")
        run = subprocess.run(["cargo", "run", "--quiet"], cwd=OUT, capture_output=True, text=True)
        if run.returncode == 0:
            rust_out, ok = run.stdout.strip(), True
            break
        extra = f"Previous attempt FAILED to compile:\n{(run.stderr or '')[:1200]}\nReturn a corrected fn main()."
        sys.stderr.write(f"[rust attempt {attempt+1}] compile failed; repairing\n")
        rust_out = "<COMPILE FAILED>"
finally:
    MAIN.write_text(backup)

print("=== AUTO-GENERATED SOURCE DRIVER (python) ===\n" + src_driver)
print("\n=== AUTO-GENERATED RUST DRIVER ===\n" + generated)
print("\n=== GOLDEN (python ran) ===\n" + (golden_out or golden.stderr.strip()))
print("\n=== RUST (ran) ===\n" + rust_out)
print("\n=== RESULT:", "✅ FULL LOOP — both drivers auto-generated, output matches"
      if (ok and golden_out and golden_out == rust_out) else "❌ no match (see above)", "===")
