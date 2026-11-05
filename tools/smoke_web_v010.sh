#!/usr/bin/env bash
set -euo pipefail
BASE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-$BASE_DIR/target/release/salamander-rs-web}"
PORT="${SALAMANDER_SMOKE_PORT:-28090}"
if [[ ! -x "$BIN" ]]; then echo "Chybi binarka: $BIN" >&2; exit 1; fi
TMP="$(mktemp -d /tmp/salamander-smoke-v010.XXXXXX)"
mkdir -p "$TMP/data" "$TMP/models"
PID=""
cleanup() { [[ -z "$PID" ]] || { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; }; rm -rf -- "$TMP"; }
trap cleanup EXIT
start_server() {
  SALAMANDER_DATA_ROOT="$TMP/data" SALAMANDER_MODELS_ROOT="$TMP/models" \
  SALAMANDER_ENABLE_WRITES=1 SALAMANDER_PORT="$PORT" "$BIN" > "$TMP/server.log" 2>&1 & PID=$!
  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:$PORT/api/health" >/dev/null; then return; fi
    if ! kill -0 "$PID" 2>/dev/null; then cat "$TMP/server.log" >&2; exit 1; fi
    sleep .2
  done
  cat "$TMP/server.log" >&2;exit 1
}
stop_server() { kill "$PID"; wait "$PID" 2>/dev/null || true; PID=""; }
start_server
url="http://127.0.0.1:$PORT"
python3 - "$TMP/data/metadata.png" <<'PY'
import struct,zlib,sys,json
p=sys.argv[1]
def chunk(typ,payload): return struct.pack('>I',len(payload))+typ+payload+struct.pack('>I',zlib.crc32(typ+payload)&0xffffffff)
png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,2,0,0,0))
png+=chunk(b'tEXt',b'parameters\x00prompt\nSteps: 25, Seed: 1234')
png+=chunk(b'iTXt',b'workflow\x00\x00\x00\x00\x00'+json.dumps({'nodes':[]}).encode())
png+=chunk(b'IDAT',zlib.compress(b'\x00\xff\x00\x00'))+chunk(b'IEND',b'')
open(p,'wb').write(png)
PY
curl -fsS "$url/api/preview/info?root=data&path=&name=metadata.png" > "$TMP/preview.json"
python3 - "$TMP/preview.json" <<'PY'
import json,sys
p=json.load(open(sys.argv[1]));keys=[i['key'] for i in p['metadata']]
assert p['kind']=='image' and 'parameters' in keys and 'workflow' in keys,p
PY
printf 'ABCDEFGHabcdefgh01234567' > "$TMP/input.bin"
fingerprint="$(printf '%064d' 0)"
python3 - "$TMP" "$url" "$fingerprint" <<'PY'
import sys,json,urllib.request
root,url,fingerprint=sys.argv[1:]
payload=json.dumps(dict(root='data',path='',name='resumed.bin',expected_size=24,last_modified=123,fingerprint=fingerprint)).encode()
r=urllib.request.Request(url+'/api/resumable/start',data=payload,headers={'Content-Type':'application/json','X-Salamander-Request':'resume-start'})
m=json.load(urllib.request.urlopen(r));open(root+'/id','w').write(m['id'])
PY
id="$(cat "$TMP/id")"
head -c 8 "$TMP/input.bin" | curl -fsS -X POST "$url/api/resumable/chunk?root=data&path=&id=$id&offset=0" \
   -H 'Content-Type: application/octet-stream' -H 'X-Salamander-Request: resume-chunk' --data-binary @- > /dev/null
stop_server
start_server
curl -fsS "$url/api/resumable/status?root=data&path=&id=$id" | python3 -c 'import sys,json; assert json.load(sys.stdin)["committed"]==8'
tail -c +9 "$TMP/input.bin" | curl -fsS -X POST "$url/api/resumable/chunk?root=data&path=&id=$id&offset=8" \
   -H 'Content-Type: application/octet-stream' -H 'X-Salamander-Request: resume-chunk' --data-binary @- > /dev/null
curl -fsS "$url/api/resumable/finish" -H 'Content-Type: application/json' -H 'X-Salamander-Request: resume-finish' \
   --data "{\"root\":\"data\",\"path\":\"\",\"id\":\"$id\"}" >/dev/null
cmp "$TMP/input.bin" "$TMP/data/resumed.bin"
curl -fsS -H 'Range: bytes=8-15' "$url/api/download?root=data&path=&name=resumed.bin" > "$TMP/range.bin"
head -c 16 "$TMP/input.bin" | tail -c 8 > "$TMP/expected-range.bin"
cmp "$TMP/range.bin" "$TMP/expected-range.bin"
echo 'OK: obnovitelny upload pres restart, HTTP Range, PNG metadata.'
