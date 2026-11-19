# The file list for scripts/nsite/publish.html: every file of a reproducible page build, with its sha256 and content type.
# Usage: python3 -I files.py <built site folder> > files.json   (run by scripts/publish-nsite.sh)
import hashlib, json, os, sys

root = sys.argv[1]
types = {'.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css',
         '.svg': 'image/svg+xml', '.wasm': 'application/wasm', '.json': 'application/json',
         '.png': 'image/png', '.ico': 'image/x-icon', '.txt': 'text/plain', '.md': 'text/markdown', '.ts': 'text/plain'}
out = []
for d, _, names in os.walk(root):
    for n in sorted(names):
        p = os.path.join(d, n)
        rel = '/' + os.path.relpath(p, root).replace(os.sep, '/')
        ext = os.path.splitext(n)[1]
        if rel == '/SHA256SUMS':  # NIP-5A paths need an extension; the manifest's path tags carry the same hashes
            continue
        if ext not in types:
            sys.exit(f'no content type for {rel}')
        data = open(p, 'rb').read()
        out.append({'path': rel, 'sha256': hashlib.sha256(data).hexdigest(), 'type': types[ext], 'size': len(data)})
out.sort(key=lambda f: f['path'])
json.dump(out, sys.stdout, indent=1)
